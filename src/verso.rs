use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, atomic::Ordering},
};

use arboard::Clipboard;
use base::id::{PipelineNamespace, PipelineNamespaceId, WebViewId};
use bluetooth::BluetoothThreadFactory;
use bluetooth_traits::BluetoothRequest;
use canvas::canvas_paint_thread::CanvasPaintThread;
use compositing_traits::{
    CompositorMsg, CompositorProxy, CrossProcessCompositorApi, WebrenderExternalImageHandlers,
    WebrenderImageHandlerType,
};
use constellation::{Constellation, FromEmbedderLogger, InitialConstellationState};
use constellation_traits::EmbedderToConstellationMessage;
use crossbeam_channel::{Receiver, Sender, unbounded};
use devtools;
use embedder_traits::{
    AllowOrDeny, EmbedderMsg, EmbedderProxy, EventLoopWaker, WebResourceResponse,
    WebResourceResponseMsg, user_content_manager::UserContentManager,
};
use euclid::Scale;
use fonts::SystemFontService;
use ipc_channel::ipc::{self, IpcSender};
use ipc_channel::router::ROUTER;
use layout;
use log::{Log, Metadata, Record};
use net::resource_thread;
use profile;
use script::{self, JSEngineSetup};
use servo_config::{opts, pref};
use servo_url::ServoUrl;
use style;
use versoview_messages::{PositionType, SizeType, ToControllerMessage, ToVersoMessage};
use webgpu;
use webrender::{ShaderPrecacheFlags, WebRenderOptions, create_webrender_instance};
use webrender_api::*;
use winit::{
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    window::WindowId,
};

use crate::{
    bookmark::BookmarkManager,
    compositor::{IOCompositor, InitialCompositorState, ShutdownState},
    config::{Config, parse_cli_args, to_winit_theme, to_winit_window_level},
    webview::execute_script,
    window::Window,
};

/// Main entry point of Verso browser.
pub struct Verso {
    windows: HashMap<WindowId, (Window, DocumentId)>,
    compositor: Option<IOCompositor>,
    constellation_sender: Sender<EmbedderToConstellationMessage>,
    to_controller_sender: Option<IpcSender<ToControllerMessage>>,
    embedder_receiver: Receiver<EmbedderMsg>,
    /// For single-process Servo instances, this field controls the initialization
    /// and deinitialization of the JS Engine. Multiprocess Servo instances have their
    /// own instance that exists in the content process instead.
    _js_engine_setup: Option<JSEngineSetup>,
    /// FIXME: It's None on wayland in Flatpak. Find a way to support this.
    clipboard: Option<Clipboard>,
    config: Config,
    bookmark_manager: BookmarkManager,
}

impl Verso {
    /// Create a Verso instance from Winit's window and event loop proxy.
    ///
    /// Following threads will be created while initializing Verso based on configurations:
    /// - Time Profiler: Enabled
    /// - Memory Profiler: Enabled
    /// - DevTools: `pref!(devtools_server_enabled)`
    /// - Webrender: Enabled
    /// - WebGL: Disabled
    /// - WebXR: Disabled
    /// - Bluetooth: Enabled
    /// - Resource: Enabled
    /// - Storage: Enabled
    /// - Font Cache: Enabled
    /// - Canvas: Enabled
    /// - Constellation: Enabled
    /// - Image Cache: Enabled
    pub fn new(evl: &ActiveEventLoop, proxy: EventLoopProxy<EventLoopProxyMessage>) -> Self {
        let (config, to_controller_sender) = try_connect_ipc_and_get_config(&proxy);

        // Initialize configurations and Verso window
        let protocols = config.create_protocols();
        let initial_url = config.url.clone();
        let with_panel = config.with_panel;
        let window_settings = config.window_attributes.clone();
        let user_scripts = config.user_scripts.clone();
        let zoom_level = config.zoom_level;

        config.init();
        // Reserving a namespace to create WebViewId.
        PipelineNamespace::install(PipelineNamespaceId(0));
        let (mut window, rendering_context) = Window::new(evl, window_settings);
        let event_loop_waker = Box::new(Waker(proxy));
        let opts = opts::get();

        // Set Stylo flags
        style::context::DEFAULT_DISABLE_STYLE_SHARING_CACHE
            .store(opts.debug.disable_share_style_cache, Ordering::Relaxed);
        style::context::DEFAULT_DUMP_STYLE_STATISTICS
            .store(opts.debug.dump_style_statistics, Ordering::Relaxed);
        style::traversal::IS_SERVO_NONINCREMENTAL_LAYOUT
            .store(opts.nonincremental_layout, Ordering::Relaxed);

        // Initialize servo media with dummy backend
        // This will create a thread to initialize a global static of servo media.
        // The thread will be closed once the static is initialized.
        // TODO: This is used by content process. Spawn it there once if we have multiprocess mode.
        servo_media::ServoMedia::init::<servo_media_dummy::DummyBackend>();

        // Get GL bindings
        let webrender_gl = rendering_context.gl.clone();

        // Create profiler threads
        let time_profiler_sender = profile::time::Profiler::create(
            &opts.time_profiling,
            opts.time_profiler_trace_path.clone(),
        );
        let mem_profiler_sender = profile::mem::Profiler::create();

        // Create compositor and embedder channels
        let (compositor_proxy, compositor_receiver) =
            create_compositor_channel(event_loop_waker.clone());
        let (embedder_proxy, embedder_receiver) = create_embedder_channel(event_loop_waker.clone());

        // Create dev tools thread
        let devtools_sender = if pref!(devtools_server_enabled) {
            Some(devtools::start_server(
                pref!(devtools_server_port) as u16,
                embedder_proxy.clone(),
            ))
        } else {
            None
        };

        // Create Webrender threads
        let (mut webrender, webrender_api_sender) = {
            let mut debug_flags = DebugFlags::empty();
            debug_flags.set(DebugFlags::PROFILER_DBG, opts.debug.webrender_stats);

            let render_notifier = Box::new(RenderNotifier::new(compositor_proxy.clone()));
            let clear_color = ColorF::new(0., 0., 0., 0.);
            create_webrender_instance(
                webrender_gl.clone(),
                render_notifier,
                WebRenderOptions {
                    // We force the use of optimized shaders here because rendering is broken
                    // on Android emulators with unoptimized shaders. This is due to a known
                    // issue in the emulator's OpenGL emulation layer.
                    // See: https://github.com/servo/servo/issues/31726
                    use_optimized_shaders: true,
                    resource_override_path: opts.shaders_dir.clone(),
                    debug_flags,
                    precache_flags: if pref!(gfx_precache_shaders) {
                        ShaderPrecacheFlags::FULL_COMPILE
                    } else {
                        ShaderPrecacheFlags::empty()
                    },
                    enable_aa: pref!(gfx_text_antialiasing_enabled),
                    enable_subpixel_aa: pref!(gfx_subpixel_text_antialiasing_enabled),
                    allow_texture_swizzling: pref!(gfx_texture_swizzling_enabled),
                    clear_color,
                    ..Default::default()
                },
                None,
            )
            .expect("Unable to initialize webrender!")
        };
        let webrender_api = webrender_api_sender.create_api();
        let webrender_document = webrender_api
            .add_document_with_id(window.size().to_i32(), u64::from(window.id()) as u32);

        // Initialize js engine if it's single process mode
        let js_engine_setup = if !opts.multiprocess {
            Some(script::init())
        } else {
            None
        };

        let (external_image_handlers, external_images) = WebrenderExternalImageHandlers::new();
        let mut external_image_handlers = Box::new(external_image_handlers);
        // Create the webgl thread
        // TODO: create webGL thread based on pref
        // let gl_type = match webrender_gl.get_type() {
        //     gl::GlType::Gl => sparkle::gl::GlType::Gl,
        //     gl::GlType::Gles => sparkle::gl::GlType::Gles,
        // };
        // let WebGLComm {
        //     webgl_threads,
        //     webxr_layer_grand_manager,
        //     image_handler,
        // } = WebGLComm::new(
        //     rendering_context.clone(),
        //     webrender_api.create_sender(),
        //     webrender_document,
        //     external_images.clone(),
        //     gl_type,
        // );
        // Set webrender external image handler for WebGL textures
        // external_image_handlers.set_handler(image_handler, WebrenderImageHandlerType::WebGL);

        // Set webrender external image handler for WebGPU textures
        let wgpu_image_handler = webgpu::WGPUExternalImages::default();
        external_image_handlers.set_handler(
            Box::new(wgpu_image_handler),
            WebrenderImageHandlerType::WebGPU,
        );

        webrender.set_external_image_handler(external_image_handlers);

        // Create bluetooth thread
        let bluetooth_thread: IpcSender<BluetoothRequest> =
            BluetoothThreadFactory::new(embedder_proxy.clone());

        // Create resource thread pool
        let (public_resource_threads, private_resource_threads) =
            resource_thread::new_resource_threads(
                devtools_sender.clone(),
                time_profiler_sender.clone(),
                mem_profiler_sender.clone(),
                embedder_proxy.clone(),
                opts.config_dir.clone(),
                opts.certificate_path.clone(),
                opts.ignore_certificate_errors,
                Arc::new(protocols),
            );

        // Create font cache thread
        let system_font_service = Arc::new(
            SystemFontService::spawn(
                compositor_proxy.cross_process_compositor_api.clone(),
                mem_profiler_sender.clone(),
            )
            .to_proxy(),
        );

        // Create canvas thread
        let (canvas_create_sender, canvas_ipc_sender) = CanvasPaintThread::start(
            compositor_proxy.cross_process_compositor_api.clone(),
            system_font_service.clone(),
            public_resource_threads.clone(),
        );

        let mut user_content_manager = UserContentManager::new();
        for script in user_scripts {
            user_content_manager.add_script(script);
        }

        // Create layout factory
        let layout_factory = Arc::new(layout::LayoutFactoryImpl());
        let initial_state = InitialConstellationState {
            compositor_proxy: compositor_proxy.clone(),
            embedder_proxy,
            devtools_sender,
            bluetooth_thread,
            system_font_service,
            public_resource_threads,
            private_resource_threads,
            time_profiler_chan: time_profiler_sender.clone(),
            mem_profiler_chan: mem_profiler_sender.clone(),
            webrender_document,
            webrender_api_sender,
            webxr_registry: None,
            webgl_threads: None,
            webrender_external_images: external_images,
            user_content_manager,
        };

        // Create constellation thread
        let constellation_sender =
            Constellation::<script::ScriptThread, script::ServiceWorkerManager>::start(
                initial_state,
                layout_factory,
                opts.random_pipeline_closure_probability,
                opts.random_pipeline_closure_seed,
                opts.hard_fail,
                canvas_create_sender,
                canvas_ipc_sender,
            );

        // Create webdriver thread
        if let Some(port) = opts.webdriver_port {
            webdriver_server::start_server(port, constellation_sender.clone());
        }

        // The compositor coordinates with the client window to create the final
        // rendered page and display it somewhere.
        let mut compositor = IOCompositor::new(
            window.id(),
            window.size(),
            Scale::new(window.scale_factor() as f32),
            InitialCompositorState {
                sender: compositor_proxy,
                receiver: compositor_receiver,
                constellation_chan: constellation_sender.clone(),
                time_profiler_chan: time_profiler_sender,
                mem_profiler_chan: mem_profiler_sender,
                webrender,
                webrender_document,
                webrender_api,
                rendering_context,
                webrender_gl,
            },
            opts.wait_for_stable_image,
            opts.debug.convert_mouse_to_touch,
        );

        if let Some(zoom_level) = zoom_level {
            compositor.on_zoom_window_event(zoom_level, &window);
        }

        if with_panel {
            window.create_panel(&constellation_sender, initial_url);
        } else {
            window.create_tab(&constellation_sender, initial_url.into());
        }

        let mut windows = HashMap::new();
        windows.insert(window.id(), (window, webrender_document));

        // Create Verso instance
        let verso = Verso {
            windows,
            compositor: Some(compositor),
            constellation_sender,
            to_controller_sender,
            embedder_receiver,
            _js_engine_setup: js_engine_setup,
            clipboard: Clipboard::new().ok(),
            config,
            bookmark_manager: BookmarkManager::new(),
        };

        verso.setup_logging();
        verso
    }

    /// Handle Winit window events. The strategy to handle event are different between platforms
    /// because the order of events might be different.
    pub fn handle_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        #[cfg(linux)]
        if let WindowEvent::Resized(_) = event {
            self.handle_winit_window_event(window_id, event);
        } else {
            self.handle_winit_window_event(window_id, event);
            self.handle_servo_messages(event_loop);
        }

        #[cfg(apple)]
        if let WindowEvent::RedrawRequested = event {
            let resizing = self.handle_winit_window_event(window_id, event);
            if !resizing {
                self.handle_servo_messages(event_loop);
            }
        } else {
            self.handle_winit_window_event(window_id, event);
            self.handle_servo_messages(event_loop);
        }

        #[cfg(windows)]
        {
            self.handle_winit_window_event(window_id, event);
            self.handle_servo_messages(event_loop);
        }
    }

    /// Handle Winit window events
    fn handle_winit_window_event(&mut self, window_id: WindowId, event: WindowEvent) -> bool {
        log::trace!("Verso is handling Winit event: {event:?}");

        let Some(compositor) = &mut self.compositor else {
            return false;
        };
        let Some((window, _)) = self.windows.get_mut(&window_id) else {
            return false;
        };

        if let WindowEvent::CloseRequested = event {
            if let Some(to_controller_sender) = &self.to_controller_sender {
                if window.event_listeners.on_close_requested {
                    if let Err(error) =
                        to_controller_sender.send(ToControllerMessage::OnCloseRequested)
                    {
                        log::error!(
                            "Verso failed to send WebResourceRequested to controller: {error}"
                        );
                    } else {
                        return false;
                    }
                }
            }
            // self.windows.remove(&window_id);
            compositor.start_shutting_down();
        } else {
            window.handle_winit_window_event(&self.constellation_sender, compositor, &event);
            return window.resizing;
        }

        false
    }

    /// Handle message came from Servo.
    pub fn handle_servo_messages(&mut self, evl: &ActiveEventLoop) {
        if self.compositor.is_none() {
            log::error!("Verso shouldn't be handling messages after compositor has shut down");
            return;
        }
        let compositor = self.compositor.as_mut().unwrap();

        // Handle Compositor's messages first
        log::trace!("Verso is handling Compositor messages");

        compositor.receive_messages(&mut self.windows);

        // Only handle incoming embedder messages if the compositor hasn't already started shutting down.
        while let Ok(msg) = self.embedder_receiver.try_recv() {
            if let Some(webview_id) = Self::get_embedder_message_webview_id(&msg) {
                if let Some((window, document_id)) = self
                    .windows
                    .values_mut()
                    .find(|(window, _)| window.has_webview(*webview_id))
                {
                    if window.handle_servo_message(
                        *webview_id,
                        msg,
                        &self.constellation_sender,
                        &self.to_controller_sender,
                        self.clipboard.as_mut(),
                        compositor,
                        &mut self.bookmark_manager,
                    ) {
                        let mut window = Window::new_with_compositor(
                            evl,
                            self.config.window_attributes.clone(),
                            compositor,
                        );
                        window.create_panel(&self.constellation_sender, self.config.url.clone());
                        let webrender_document = *document_id;
                        self.windows
                            .insert(window.id(), (window, webrender_document));
                    }
                }
            } else {
                // Handle message in Verso Window
                log::trace!("Verso Window is handling Embedder message: {msg:?}");
                match msg {
                    EmbedderMsg::OnDevtoolsStarted(port, _token) => {
                        if let Ok(port) = port {
                            // We use level error by default so this won't show
                            // log::info!("Devtools server listening on port {port}");
                            println!("Devtools server listening on port {port}");
                        } else {
                            log::error!("Failed to start devtools server");
                        }
                    }
                    EmbedderMsg::RequestDevtoolsConnection(sender) => {
                        if let Err(err) = sender.send(AllowOrDeny::Allow) {
                            log::error!(
                                "Failed to send RequestDevtoolsConnection response back: {err}"
                            );
                        }
                    }
                    EmbedderMsg::ShutdownComplete => {
                        compositor.finish_shutting_down();
                    }
                    e => {
                        log::trace!(
                            "Verso Window isn't supporting handling this message yet: {e:?}"
                        )
                    }
                }
            }

            if compositor.shutdown_state == ShutdownState::FinishedShuttingDown {
                break;
            }
        }

        compositor.perform_updates(&mut self.windows);

        if compositor.needs_repaint() {
            if let Some(window) = self.windows.get(&compositor.current_window) {
                window.0.request_redraw();
            }
        }

        // Check if Verso need to start shutting down.
        if self.windows.is_empty() {
            compositor.start_shutting_down();
        }

        let shutdown = compositor.shutdown_state == ShutdownState::FinishedShuttingDown;

        // Check compositor status and set control flow.
        if shutdown {
            // If Compositor has shut down, deinit and remove it.
            if let Some(mut compositor) = self.compositor.take() {
                IOCompositor::deinit(&mut compositor)
            }
            evl.exit();
        } else if self.is_animating() {
            evl.set_control_flow(ControlFlow::Poll);
        } else {
            evl.set_control_flow(ControlFlow::Wait);
        }
    }

    fn get_embedder_message_webview_id(msg: &EmbedderMsg) -> Option<&WebViewId> {
        match msg {
            EmbedderMsg::Status(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ChangePageTitle(webview_id, ..) => Some(webview_id),
            EmbedderMsg::MoveTo(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ResizeTo(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowSimpleDialog(webview_id, ..) => Some(webview_id),
            EmbedderMsg::RequestAuthentication(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowContextMenu(webview_id, ..) => Some(webview_id),
            EmbedderMsg::AllowNavigationRequest(webview_id, ..) => Some(webview_id),
            EmbedderMsg::AllowOpeningWebView(webview_id, ..) => Some(webview_id),
            EmbedderMsg::WebViewClosed(webview_id) => Some(webview_id),
            EmbedderMsg::WebViewFocused(webview_id) => Some(webview_id),
            EmbedderMsg::WebViewBlurred => None,
            EmbedderMsg::AllowUnload(webview_id, ..) => Some(webview_id),
            EmbedderMsg::Keyboard(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ClearClipboard(webview_id) => Some(webview_id),
            EmbedderMsg::GetClipboardText(webview_id, ..) => Some(webview_id),
            EmbedderMsg::SetClipboardText(webview_id, ..) => Some(webview_id),
            EmbedderMsg::SetCursor(webview_id, ..) => Some(webview_id),
            EmbedderMsg::NewFavicon(webview_id, ..) => Some(webview_id),
            EmbedderMsg::HistoryChanged(webview_id, ..) => Some(webview_id),
            EmbedderMsg::NotifyFullscreenStateChanged(webview_id, ..) => Some(webview_id),
            EmbedderMsg::NotifyLoadStatusChanged(webview_id, ..) => Some(webview_id),
            EmbedderMsg::WebResourceRequested(opt_webview_id, ..) => opt_webview_id.as_ref(),
            EmbedderMsg::Panic(webview_id, ..) => Some(webview_id),
            EmbedderMsg::GetSelectedBluetoothDevice(webview_id, ..) => Some(webview_id),
            EmbedderMsg::SelectFiles(webview_id, ..) => Some(webview_id),
            EmbedderMsg::PromptPermission(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowIME(webview_id, ..) => Some(webview_id),
            EmbedderMsg::HideIME(webview_id) => Some(webview_id),
            EmbedderMsg::ReportProfile(..) => None,
            EmbedderMsg::MediaSessionEvent(webview_id, ..) => Some(webview_id),
            EmbedderMsg::OnDevtoolsStarted(..) => None,
            EmbedderMsg::RequestDevtoolsConnection(..) => None,
            EmbedderMsg::PlayGamepadHapticEffect(webview_id, ..) => Some(webview_id),
            EmbedderMsg::StopGamepadHapticEffect(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowNotification(opt_webview_id, ..) => opt_webview_id.as_ref(),
            EmbedderMsg::ShowFormControl(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShutdownComplete => None,
            EmbedderMsg::FinishJavaScriptEvaluation(..) => None,
        }
    }

    /// Request Verso to redraw. It will queue a redraw event on current focused window.
    pub fn request_redraw(&mut self, evl: &ActiveEventLoop) {
        let Some(compositor) = &mut self.compositor else {
            return;
        };

        if let Some((window, _)) = self.windows.get(&compositor.current_window) {
            // evl.set_control_flow(ControlFlow::Poll);
            window.request_redraw();

            // Wait for `request_redraw` to trigger the next event loop if the window is visible
            if window.window.is_visible().unwrap_or(true) {
                return;
            }
        }

        self.handle_servo_messages(evl);
    }

    /// Handle message came from webview controller.
    pub fn handle_incoming_webview_message(
        &mut self,
        event_loop: &ActiveEventLoop,
        message: ToVersoMessage,
    ) {
        match message {
            ToVersoMessage::Exit => {
                if let Some(compositor) = &mut self.compositor {
                    compositor.start_shutting_down();
                    self.handle_servo_messages(event_loop);
                }
            }
            ToVersoMessage::ListenToOnCloseRequested => {
                if let Some(window) = self.first_window_mut() {
                    window.event_listeners.on_close_requested = true;
                }
            }
            ToVersoMessage::NavigateTo(to_url) => {
                if let Some(webview_id) = self.first_webview_id() {
                    send_to_constellation(
                        &self.constellation_sender,
                        EmbedderToConstellationMessage::LoadUrl(
                            webview_id,
                            ServoUrl::from_url(to_url),
                        ),
                    );
                }
            }
            ToVersoMessage::Reload => {
                if let Some(webview_id) = self.first_webview_id() {
                    send_to_constellation(
                        &self.constellation_sender,
                        EmbedderToConstellationMessage::Reload(webview_id),
                    );
                }
            }
            ToVersoMessage::ListenToOnNavigationStarting => {
                if let Some(window) = self.first_window_mut() {
                    window.event_listeners.on_navigation_starting = true;
                }
            }
            ToVersoMessage::OnNavigationStartingResponse(id, allow) => {
                send_to_constellation(
                    &self.constellation_sender,
                    EmbedderToConstellationMessage::AllowNavigationResponse(
                        bincode::deserialize(&id).unwrap(),
                        allow,
                    ),
                );
            }
            ToVersoMessage::ExecuteScript(js) => {
                if let Some(webview_id) = self.first_webview_id() {
                    let _ = execute_script(&self.constellation_sender, &webview_id, js);
                }
            }
            ToVersoMessage::ListenToWebResourceRequests => {
                if let Some(window) = self.first_window_mut() {
                    window
                        .event_listeners
                        .on_web_resource_requested
                        .replace(HashMap::new());
                }
            }
            ToVersoMessage::WebResourceRequestResponse(response) => {
                if let Some(window) = self.first_window_mut() {
                    if let Some((url, sender)) = window
                        .event_listeners
                        .on_web_resource_requested
                        .as_mut()
                        .and_then(|senders| senders.remove(&response.id))
                    {
                        if let Some(response) = response.response {
                            let _ = sender
                                .send(WebResourceResponseMsg::Start(
                                    WebResourceResponse::new(url)
                                        .headers(response.headers().clone())
                                        .status_code(response.status()),
                                ))
                                .and_then(|_| {
                                    sender.send(WebResourceResponseMsg::SendBodyData(
                                        response.into_body(),
                                    ))
                                })
                                .and_then(|_| sender.send(WebResourceResponseMsg::FinishLoad));
                        } else {
                            let _ = sender.send(WebResourceResponseMsg::DoNotIntercept);
                        }
                    }
                }
            }
            ToVersoMessage::SetTitle(title) => {
                if let Some(window) = self.first_window() {
                    window.window.set_title(&title);
                }
            }
            ToVersoMessage::SetSize(size) => {
                if let Some(window) = self.first_window() {
                    let _ = window.window.request_inner_size(size);
                }
            }
            ToVersoMessage::SetPosition(position) => {
                if let Some(window) = self.first_window() {
                    window.window.set_outer_position(position);
                }
            }
            ToVersoMessage::SetMaximized(maximized) => {
                if let Some(window) = self.first_window() {
                    window.window.set_maximized(maximized);
                }
            }
            ToVersoMessage::SetMinimized(minimized) => {
                if let Some(window) = self.first_window() {
                    window.window.set_minimized(minimized);
                }
            }
            ToVersoMessage::SetFullscreen(fullscreen) => {
                if let Some(window) = self.first_window() {
                    window.window.set_fullscreen(if fullscreen {
                        Some(winit::window::Fullscreen::Borderless(None))
                    } else {
                        None
                    });
                }
            }
            ToVersoMessage::SetVisible(visible) => {
                if let Some(window) = self.first_window() {
                    window.window.set_visible(visible);
                }
            }
            ToVersoMessage::SetWindowLevel(window_level) => {
                if let Some(window) = self.first_window() {
                    window
                        .window
                        .set_window_level(to_winit_window_level(window_level));
                }
            }
            ToVersoMessage::SetTheme(theme) => {
                if let Some(window) = self.first_window() {
                    window.window.set_theme(to_winit_theme(&theme));
                }
            }
            ToVersoMessage::StartDragging => {
                if let Some(window) = self.first_window() {
                    let _ = window.window.drag_window();
                }
            }
            ToVersoMessage::Focus => {
                if let Some(window) = self.first_window() {
                    window.window.focus_window();
                }
            }
            ToVersoMessage::GetTitle(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetTitleResponse(id, window.window.title()),
                    ) {
                        log::error!("Verso failed to send GetTitleReponse to controller: {error}")
                    }
                }
            }
            ToVersoMessage::GetSize(id, size_type) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetSizeResponse(
                            id,
                            match size_type {
                                SizeType::Inner => window.window.inner_size(),
                                SizeType::Outer => window.window.outer_size(),
                            },
                        ),
                    ) {
                        log::error!("Verso failed to send GetSizeReponse to controller: {error}")
                    }
                }
            }
            ToVersoMessage::GetPosition(id, position_type) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetPositionResponse(
                            id,
                            match position_type {
                                PositionType::Inner => window.window.inner_position(),
                                PositionType::Outer => window.window.outer_position(),
                            }
                            .ok(),
                        ),
                    ) {
                        log::error!(
                            "Verso failed to send GetPositionResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::GetMinimized(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetMinimizedResponse(
                            id,
                            window.window.is_minimized().unwrap_or_default(),
                        ),
                    ) {
                        log::error!(
                            "Verso failed to send GetMinimizedResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::GetMaximized(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetMaximizedResponse(id, window.window.is_maximized()),
                    ) {
                        log::error!(
                            "Verso failed to send GetMaximizedResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::GetFullscreen(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetFullscreenResponse(
                            id,
                            window.window.fullscreen().is_some(),
                        ),
                    ) {
                        log::error!(
                            "Verso failed to send GetFullscreenResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::GetVisible(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetVisibleResponse(
                            id,
                            window.window.is_visible().unwrap_or(true),
                        ),
                    ) {
                        log::error!(
                            "Verso failed to send GetVisibleResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::GetScaleFactor(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetScaleFactorResponse(
                            id,
                            window.window.scale_factor(),
                        ),
                    ) {
                        log::error!(
                            "Verso failed to send GetScaleFactorResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::GetTheme(id) => {
                if let Some(window) = self.first_window() {
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetThemeResponse(
                            id,
                            match window.window.theme() {
                                Some(winit::window::Theme::Dark) => versoview_messages::Theme::Dark,
                                _ => versoview_messages::Theme::Light,
                            },
                        ),
                    ) {
                        log::error!("Verso failed to send GetThemeResponse to controller: {error}")
                    }
                }
            }
            ToVersoMessage::GetCurrentUrl(id) => {
                if let Some(window) = self.first_window() {
                    let tab = window.tab_manager.current_tab().unwrap();
                    let history = tab.history();
                    if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                        ToControllerMessage::GetCurrentUrlResponse(
                            id,
                            history.list[history.current_idx].as_url().clone(),
                        ),
                    ) {
                        log::error!(
                            "Verso failed to send GetScaleFactorResponse to controller: {error}"
                        )
                    }
                }
            }
            ToVersoMessage::SetConfig(..) => {
                log::error!(
                    "`ToVersoMessage::SetConfig` should not be received after the initial setup"
                )
            }
        }
    }

    fn first_window(&self) -> Option<&Window> {
        self.windows.values().next().map(|(window, _)| window)
    }

    fn first_window_mut(&mut self) -> Option<&mut Window> {
        self.windows.values_mut().next().map(|(window, _)| window)
    }

    fn first_webview_id(&self) -> Option<WebViewId> {
        self.windows
            .values()
            .next()
            .and_then(|(window, _)| window.tab_manager.current_tab().map(|tab| tab.id()))
    }

    /// Return true if one of the Verso windows is animating.
    pub fn is_animating(&self) -> bool {
        self.compositor
            .as_ref()
            .map(|c| c.is_animating)
            .unwrap_or(false)
    }

    fn setup_logging(&self) {
        let constellation_chan = self.constellation_sender.clone();
        let env = env_logger::Env::default();
        let env_logger = env_logger::Builder::from_env(env).build();
        let con_logger = FromEmbedderLogger::new(constellation_chan);

        let filter = std::cmp::max(env_logger.filter(), con_logger.filter());
        let logger = BothLogger(env_logger, con_logger);

        log::set_boxed_logger(Box::new(logger)).expect("Failed to set logger.");
        log::set_max_level(filter);
    }
}

/// Parse the command line arguments,
/// if `ipc_channel` is set, we try to connect to it and set up routing to the event loop proxy
/// then return the config from [`ToVersoMessage::SetConfig`] or fallback to from the command line arguments
fn try_connect_ipc_and_get_config(
    proxy: &EventLoopProxy<EventLoopProxyMessage>,
) -> (Config, Option<IpcSender<ToControllerMessage>>) {
    let cli_args = parse_cli_args().unwrap_or_default();
    let (to_controller_sender, initial_settings) = if let Some(ipc_channel) = &cli_args.ipc_channel
    {
        let sender = IpcSender::<ToControllerMessage>::connect(ipc_channel.to_string()).unwrap();
        let (to_verso_sender, receiver) = ipc::channel::<ToVersoMessage>().unwrap();
        sender
            .send(ToControllerMessage::SetToVersoSender(to_verso_sender))
            .unwrap();
        let ToVersoMessage::SetConfig(initial_settings) = receiver
            .recv()
            .expect("Failed to recieve the initial settings from controller")
        else {
            panic!("The initial message sent from versoview is not a `ToVersoMessage::SetConfig`")
        };
        let proxy_clone = EventLoopProxyDropGuard(proxy.clone());
        ROUTER.add_typed_route(
            receiver,
            Box::new(move |message| match message {
                Ok(message) => {
                    if let Err(e) = proxy_clone
                        .0
                        .send_event(EventLoopProxyMessage::IpcMessage(Box::new(message)))
                    {
                        log::error!("Failed to send controller message to Verso: {e}");
                    }
                }
                Err(e) => log::error!("Failed to receive controller message: {e}"),
            }),
        );
        (Some(sender), Some(initial_settings))
    } else {
        (None, None)
    };
    let config = if let Some(initial_settings) = initial_settings {
        Config::from_controller_config(initial_settings)
    } else {
        Config::from_cli_args(cli_args)
    };
    (config, to_controller_sender)
}

/// Signal the event loop to exit when dropped,
/// this is used for when we lose the connection with our host controller
struct EventLoopProxyDropGuard(EventLoopProxy<EventLoopProxyMessage>);

impl Drop for EventLoopProxyDropGuard {
    fn drop(&mut self) {
        if let Err(error) = self
            .0
            .send_event(EventLoopProxyMessage::IpcMessage(Box::new(
                ToVersoMessage::Exit,
            )))
        {
            log::error!("Failed to send exit message to event loop on IPC disconnect: {error}");
        }
    }
}

/// Message send to the event loop
#[derive(Debug)]
pub enum EventLoopProxyMessage {
    /// Wake
    Wake,
    /// Message coming from the webview controller
    IpcMessage(Box<ToVersoMessage>),
}

#[derive(Debug, Clone)]
struct Waker(pub EventLoopProxy<EventLoopProxyMessage>);

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        if let Err(e) = self.0.send_event(EventLoopProxyMessage::Wake) {
            log::error!("Servo failed to send wake up event to Verso: {e}");
        }
    }
}

#[derive(Clone)]
struct RenderNotifier {
    compositor_proxy: CompositorProxy,
}

impl RenderNotifier {
    pub fn new(compositor_proxy: CompositorProxy) -> RenderNotifier {
        RenderNotifier { compositor_proxy }
    }
}

impl webrender::api::RenderNotifier for RenderNotifier {
    fn clone(&self) -> Box<dyn webrender::api::RenderNotifier> {
        Box::new(RenderNotifier::new(self.compositor_proxy.clone()))
    }

    fn wake_up(&self, _composite_needed: bool) {}

    fn new_frame_ready(
        &self,
        document_id: DocumentId,
        _publish_id: FramePublishId,
        params: &FrameReadyParams,
    ) {
        self.compositor_proxy
            .send(CompositorMsg::NewWebRenderFrameReady(
                document_id,
                params.render,
            ));
    }
}

// A logger that logs to two downstream loggers.
// This should probably be in the log crate.
struct BothLogger<Log1, Log2>(Log1, Log2);

impl<Log1, Log2> Log for BothLogger<Log1, Log2>
where
    Log1: Log,
    Log2: Log,
{
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.0.enabled(metadata) || self.1.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.0.log(record);
        self.1.log(record);
    }

    fn flush(&self) {
        self.0.flush();
        self.1.flush();
    }
}

pub(crate) fn send_to_constellation(
    sender: &Sender<EmbedderToConstellationMessage>,
    msg: EmbedderToConstellationMessage,
) {
    let variant_name: &str = (&msg).into();
    if let Err(e) = sender.send(msg) {
        log::warn!("Sending {variant_name} to constellation failed: {e:?}");
    }
}

fn create_embedder_channel(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (EmbedderProxy, Receiver<EmbedderMsg>) {
    let (sender, receiver) = unbounded();
    (
        EmbedderProxy {
            sender,
            event_loop_waker,
        },
        receiver,
    )
}

fn create_compositor_channel(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (CompositorProxy, Receiver<CompositorMsg>) {
    let (sender, receiver) = unbounded();

    let (compositor_ipc_sender, compositor_ipc_receiver) =
        ipc::channel().expect("ipc channel failure");

    let cross_process_compositor_api = CrossProcessCompositorApi(compositor_ipc_sender);
    let compositor_proxy = CompositorProxy {
        sender,
        cross_process_compositor_api,
        event_loop_waker,
    };

    let compositor_proxy_clone = compositor_proxy.clone();
    ROUTER.add_typed_route(
        compositor_ipc_receiver,
        Box::new(move |message| {
            compositor_proxy_clone.send(message.expect("Could not convert Compositor message"));
        }),
    );

    (compositor_proxy, receiver)
}
