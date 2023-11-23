use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use glutin::{
    display::GetGlDisplay,
    prelude::{GlDisplay, NotCurrentGlContextSurfaceAccessor, PossiblyCurrentGlContext},
    surface::GlSurface,
};
use raw_window_handle::{HasRawDisplayHandle as _, HasRawWindowHandle as _};
use winit::{
    event_loop::{EventLoop, EventLoopProxy, EventLoopWindowTarget},
    window::{Window, WindowId},
};

use egui::{
    epaint::ahash::HashMap, DeferredViewportUiCallback, ImmediateViewport, NumExt as _,
    ViewportBuilder, ViewportClass, ViewportId, ViewportIdMap, ViewportIdPair, ViewportIdSet,
    ViewportInfo, ViewportOutput,
};
#[cfg(feature = "accesskit")]
use egui_winit::accesskit_winit;
use egui_winit::{
    apply_viewport_builder_to_new_window, create_winit_window_builder, process_viewport_commands,
    EventResponse,
};

use crate::{
    native::{epi_integration::EpiIntegration, winit_integration::create_egui_context},
    App, AppCreator, CreationContext, NativeOptions, Result, Storage,
};

use super::{
    winit_integration::{EventResult, UserEvent, WinitApp},
    *,
};

// Note: that the current Glutin API design tightly couples the GL context with
// the Window which means it's not practically possible to just destroy the
// window and re-create a new window while continuing to use the same GL context.
//
// For now this means it's not possible to support Android as well as we can with
// wgpu because we're basically forced to destroy and recreate _everything_ when
// the application suspends and resumes.
//
// There is work in progress to improve the Glutin API so it has a separate Surface
// API that would allow us to just destroy a Window/Surface when suspending, see:
// https://github.com/rust-windowing/glutin/pull/1435

// ----------------------------------------------------------------------------
// Types:

pub struct GlowWinitApp {
    repaint_proxy: Arc<egui::mutex::Mutex<EventLoopProxy<UserEvent>>>,
    app_name: String,
    native_options: NativeOptions,
    running: Option<GlowWinitRunning>,

    // Note that since this `AppCreator` is FnOnce we are currently unable to support
    // re-initializing the `GlowWinitRunning` state on Android if the application
    // suspends and resumes.
    app_creator: Option<AppCreator>,
}

/// State that is initialized when the application is first starts running via
/// a Resumed event. On Android this ensures that any graphics state is only
/// initialized once the application has an associated `SurfaceView`.
struct GlowWinitRunning {
    integration: EpiIntegration,
    app: Box<dyn App>,

    // These needs to be shared with the immediate viewport renderer, hence the Rc/Arc/RefCells:
    glutin: Rc<RefCell<GlutinWindowContext>>,
    painter: Rc<RefCell<egui_glow::Painter>>,
}

/// This struct will contain both persistent and temporary glutin state.
///
/// Platform Quirks:
/// * Microsoft Windows: requires that we create a window before opengl context.
/// * Android: window and surface should be destroyed when we receive a suspend event. recreate on resume event.
///
/// winit guarantees that we will get a Resumed event on startup on all platforms.
/// * Before Resumed event: `gl_config`, `gl_context` can be created at any time. on windows, a window must be created to get `gl_context`.
/// * Resumed: `gl_surface` will be created here. `window` will be re-created here for android.
/// * Suspended: on android, we drop window + surface.  on other platforms, we don't get Suspended event.
///
/// The setup is divided between the `new` fn and `on_resume` fn. we can just assume that `on_resume` is a continuation of
/// `new` fn on all platforms. only on android, do we get multiple resumed events because app can be suspended.
struct GlutinWindowContext {
    egui_ctx: egui::Context,

    swap_interval: glutin::surface::SwapInterval,
    gl_config: glutin::config::Config,

    max_texture_side: Option<usize>,

    current_gl_context: Option<glutin::context::PossiblyCurrentContext>,
    not_current_gl_context: Option<glutin::context::NotCurrentContext>,

    viewports: ViewportIdMap<Viewport>,
    viewport_from_window: HashMap<WindowId, ViewportId>,
    window_from_viewport: ViewportIdMap<WindowId>,

    focused_viewport: Option<ViewportId>,
}

struct Viewport {
    ids: ViewportIdPair,
    class: ViewportClass,
    builder: ViewportBuilder,
    info: ViewportInfo,
    screenshot_requested: bool,

    /// The user-callback that shows the ui.
    /// None for immediate viewports.
    viewport_ui_cb: Option<Arc<DeferredViewportUiCallback>>,

    gl_surface: Option<glutin::surface::Surface<glutin::surface::WindowSurface>>,
    window: Option<Rc<Window>>,
    egui_winit: Option<egui_winit::State>,
}

// ----------------------------------------------------------------------------

impl GlowWinitApp {
    pub fn new(
        event_loop: &EventLoop<UserEvent>,
        app_name: &str,
        native_options: NativeOptions,
        app_creator: AppCreator,
    ) -> Self {
        crate::profile_function!();
        Self {
            repaint_proxy: Arc::new(egui::mutex::Mutex::new(event_loop.create_proxy())),
            app_name: app_name.to_owned(),
            native_options,
            running: None,
            app_creator: Some(app_creator),
        }
    }

    #[allow(unsafe_code)]
    fn create_glutin_windowed_context(
        egui_ctx: &egui::Context,
        event_loop: &EventLoopWindowTarget<UserEvent>,
        storage: Option<&dyn Storage>,
        native_options: &mut NativeOptions,
    ) -> Result<(GlutinWindowContext, egui_glow::Painter)> {
        crate::profile_function!();

        let window_settings = epi_integration::load_window_settings(storage);

        let winit_window_builder = epi_integration::viewport_builder(
            egui_ctx.zoom_factor(),
            event_loop,
            native_options,
            window_settings,
        );

        let mut glutin_window_context = unsafe {
            GlutinWindowContext::new(egui_ctx, winit_window_builder, native_options, event_loop)?
        };

        // Creates the window - must come before we create our glow context
        glutin_window_context.on_resume(event_loop)?;

        if let Some(viewport) = glutin_window_context.viewports.get(&ViewportId::ROOT) {
            if let Some(window) = &viewport.window {
                epi_integration::apply_window_settings(window, window_settings);
            }
        }

        let gl = unsafe {
            crate::profile_scope!("glow::Context::from_loader_function");
            Rc::new(glow::Context::from_loader_function(|s| {
                let s = std::ffi::CString::new(s)
                    .expect("failed to construct C string from string for gl proc address");

                glutin_window_context.get_proc_address(&s)
            }))
        };

        let painter = egui_glow::Painter::new(gl, "", native_options.shader_version)?;

        Ok((glutin_window_context, painter))
    }

    fn init_run_state(
        &mut self,
        event_loop: &EventLoopWindowTarget<UserEvent>,
    ) -> Result<&mut GlowWinitRunning> {
        crate::profile_function!();

        let storage = epi_integration::create_storage(
            self.native_options
                .viewport
                .app_id
                .as_ref()
                .unwrap_or(&self.app_name),
        );

        let egui_ctx = create_egui_context(storage.as_deref());

        let (mut glutin, painter) = Self::create_glutin_windowed_context(
            &egui_ctx,
            event_loop,
            storage.as_deref(),
            &mut self.native_options,
        )?;
        let gl = painter.gl().clone();

        let max_texture_side = painter.max_texture_side();
        glutin.max_texture_side = Some(max_texture_side);
        for viewport in glutin.viewports.values_mut() {
            if let Some(egui_winit) = viewport.egui_winit.as_mut() {
                egui_winit.set_max_texture_side(max_texture_side);
            }
        }

        let system_theme =
            winit_integration::system_theme(&glutin.window(ViewportId::ROOT), &self.native_options);

        let integration = EpiIntegration::new(
            egui_ctx,
            &glutin.window(ViewportId::ROOT),
            system_theme,
            &self.app_name,
            &self.native_options,
            storage,
            Some(gl.clone()),
            #[cfg(feature = "wgpu")]
            None,
        );

        {
            let event_loop_proxy = self.repaint_proxy.clone();
            integration
                .egui_ctx
                .set_request_repaint_callback(move |info| {
                    log::trace!("request_repaint_callback: {info:?}");
                    let when = Instant::now() + info.delay;
                    let frame_nr = info.current_frame_nr;
                    event_loop_proxy
                        .lock()
                        .send_event(UserEvent::RequestRepaint {
                            viewport_id: info.viewport_id,
                            when,
                            frame_nr,
                        })
                        .ok();
                });
        }

        #[cfg(feature = "accesskit")]
        {
            let event_loop_proxy = self.repaint_proxy.lock().clone();
            let viewport = glutin.viewports.get_mut(&ViewportId::ROOT).unwrap();
            if let Viewport {
                window: Some(window),
                egui_winit: Some(egui_winit),
                ..
            } = viewport
            {
                integration.init_accesskit(egui_winit, window, event_loop_proxy);
            }
        }

        let theme = system_theme.unwrap_or(self.native_options.default_theme);
        integration.egui_ctx.set_visuals(theme.egui_visuals());

        if self
            .native_options
            .viewport
            .mouse_passthrough
            .unwrap_or(false)
        {
            if let Err(err) = glutin.window(ViewportId::ROOT).set_cursor_hittest(false) {
                log::warn!("set_cursor_hittest(false) failed: {err}");
            }
        }

        let app_creator = std::mem::take(&mut self.app_creator)
            .expect("Single-use AppCreator has unexpectedly already been taken");

        let app = {
            let window = glutin.window(ViewportId::ROOT);
            let cc = CreationContext {
                egui_ctx: integration.egui_ctx.clone(),
                integration_info: integration.frame.info().clone(),
                storage: integration.frame.storage(),
                gl: Some(gl),
                #[cfg(feature = "wgpu")]
                wgpu_render_state: None,
                raw_display_handle: window.raw_display_handle(),
                raw_window_handle: window.raw_window_handle(),
            };
            crate::profile_scope!("app_creator");
            app_creator(&cc)
        };

        let glutin = Rc::new(RefCell::new(glutin));
        let painter = Rc::new(RefCell::new(painter));

        {
            // Create weak pointers so that we don't keep
            // state alive for too long.
            let glutin = Rc::downgrade(&glutin);
            let painter = Rc::downgrade(&painter);
            let beginning = integration.beginning;

            let event_loop: *const EventLoopWindowTarget<UserEvent> = event_loop;

            egui::Context::set_immediate_viewport_renderer(move |egui_ctx, immediate_viewport| {
                if let (Some(glutin), Some(painter)) = (glutin.upgrade(), painter.upgrade()) {
                    // SAFETY: the event loop lives longer than
                    // the Rc:s we just upgraded above.
                    #[allow(unsafe_code)]
                    let event_loop = unsafe { event_loop.as_ref().unwrap() };

                    render_immediate_viewport(
                        event_loop,
                        egui_ctx,
                        &glutin,
                        &painter,
                        beginning,
                        immediate_viewport,
                    );
                } else {
                    log::warn!("render_sync_callback called after window closed");
                }
            });
        }

        Ok(self.running.insert(GlowWinitRunning {
            glutin,
            painter,
            integration,
            app,
        }))
    }
}

impl WinitApp for GlowWinitApp {
    fn frame_nr(&self, viewport_id: ViewportId) -> u64 {
        self.running
            .as_ref()
            .map_or(0, |r| r.integration.egui_ctx.frame_nr_for(viewport_id))
    }

    fn is_focused(&self, window_id: WindowId) -> bool {
        if let Some(running) = &self.running {
            let glutin = running.glutin.borrow();
            if let Some(window_id) = glutin.viewport_from_window.get(&window_id) {
                return glutin.focused_viewport == Some(*window_id);
            }
        }

        false
    }

    fn integration(&self) -> Option<&EpiIntegration> {
        self.running.as_ref().map(|r| &r.integration)
    }

    fn window(&self, window_id: WindowId) -> Option<Rc<Window>> {
        let running = self.running.as_ref()?;
        let glutin = running.glutin.borrow();
        let viewport_id = *glutin.viewport_from_window.get(&window_id)?;
        if let Some(viewport) = glutin.viewports.get(&viewport_id) {
            viewport.window.clone()
        } else {
            None
        }
    }

    fn window_id_from_viewport_id(&self, id: ViewportId) -> Option<WindowId> {
        self.running
            .as_ref()
            .and_then(|r| r.glutin.borrow().window_from_viewport.get(&id).copied())
    }

    fn save_and_destroy(&mut self) {
        if let Some(mut running) = self.running.take() {
            crate::profile_function!();

            running.integration.save(
                running.app.as_mut(),
                Some(&running.glutin.borrow().window(ViewportId::ROOT)),
            );
            running.app.on_exit(Some(running.painter.borrow().gl()));
            running.painter.borrow_mut().destroy();
        }
    }

    fn run_ui_and_paint(&mut self, window_id: WindowId) -> EventResult {
        if let Some(running) = &mut self.running {
            running.run_ui_and_paint(window_id)
        } else {
            EventResult::Wait
        }
    }

    fn on_event(
        &mut self,
        event_loop: &EventLoopWindowTarget<UserEvent>,
        event: &winit::event::Event<'_, UserEvent>,
    ) -> Result<EventResult> {
        crate::profile_function!(winit_integration::short_event_description(event));

        Ok(match event {
            winit::event::Event::Resumed => {
                let running = if let Some(running) = &mut self.running {
                    // not the first resume event. create whatever you need.
                    running.glutin.borrow_mut().on_resume(event_loop)?;
                    running
                } else {
                    // first resume event.
                    // we can actually move this outside of event loop.
                    // and just run the on_resume fn of gl_window
                    self.init_run_state(event_loop)?
                };
                let window_id = running
                    .glutin
                    .borrow()
                    .window_from_viewport
                    .get(&ViewportId::ROOT)
                    .copied();
                EventResult::RepaintNow(window_id.unwrap())
            }

            winit::event::Event::Suspended => {
                if let Some(running) = &mut self.running {
                    running.glutin.borrow_mut().on_suspend()?;
                }
                EventResult::Wait
            }

            winit::event::Event::MainEventsCleared => {
                if let Some(running) = &self.running {
                    if let Err(err) = running.glutin.borrow_mut().on_resume(event_loop) {
                        log::warn!("on_resume failed {err}");
                    }
                }
                EventResult::Wait
            }

            winit::event::Event::WindowEvent { event, window_id } => {
                if let Some(running) = &mut self.running {
                    running.on_window_event(*window_id, event)
                } else {
                    EventResult::Wait
                }
            }

            #[cfg(feature = "accesskit")]
            winit::event::Event::UserEvent(UserEvent::AccessKitActionRequest(
                accesskit_winit::ActionRequestEvent { request, window_id },
            )) => {
                if let Some(running) = &self.running {
                    let mut glutin = running.glutin.borrow_mut();
                    if let Some(viewport_id) = glutin.viewport_from_window.get(window_id).copied() {
                        if let Some(viewport) = glutin.viewports.get_mut(&viewport_id) {
                            if let Some(egui_winit) = &mut viewport.egui_winit {
                                crate::profile_scope!("on_accesskit_action_request");
                                egui_winit.on_accesskit_action_request(request.clone());
                            }
                        }
                    }
                    // As a form of user input, accessibility actions should
                    // lead to a repaint.
                    EventResult::RepaintNext(*window_id)
                } else {
                    EventResult::Wait
                }
            }
            _ => EventResult::Wait,
        })
    }
}

impl GlowWinitRunning {
    fn run_ui_and_paint(&mut self, window_id: WindowId) -> EventResult {
        crate::profile_function!();

        let Some(viewport_id) = self
            .glutin
            .borrow()
            .viewport_from_window
            .get(&window_id)
            .copied()
        else {
            return EventResult::Wait;
        };

        #[cfg(feature = "puffin")]
        puffin::GlobalProfiler::lock().new_frame();

        {
            let glutin = self.glutin.borrow();
            let viewport = &glutin.viewports[&viewport_id];
            let is_immediate = viewport.viewport_ui_cb.is_none();
            if is_immediate && viewport_id != ViewportId::ROOT {
                // This will only happen if this is an immediate viewport.
                // That means that the viewport cannot be rendered by itself and needs his parent to be rendered.
                if let Some(parent_viewport) = glutin.viewports.get(&viewport.ids.parent) {
                    if let Some(window) = parent_viewport.window.as_ref() {
                        return EventResult::RepaintNext(window.id());
                    }
                }
                return EventResult::Wait;
            }
        }

        let (raw_input, viewport_ui_cb) = {
            let mut glutin = self.glutin.borrow_mut();
            let viewport = glutin.viewports.get_mut(&viewport_id).unwrap();
            viewport.update_viewport_info();
            let window = viewport.window.as_ref().unwrap();

            let egui_winit = viewport.egui_winit.as_mut().unwrap();
            let mut raw_input = egui_winit.take_egui_input(window);
            let viewport_ui_cb = viewport.viewport_ui_cb.clone();

            self.integration.pre_update();

            raw_input.time = Some(self.integration.beginning.elapsed().as_secs_f64());
            raw_input.viewports = glutin
                .viewports
                .iter()
                .map(|(id, viewport)| (*id, viewport.info.clone()))
                .collect();

            (raw_input, viewport_ui_cb)
        };

        // ------------------------------------------------------------
        // The update function, which could call immediate viewports,
        // so make sure we don't hold any locks here required by the immediate viewports rendeer.

        let full_output =
            self.integration
                .update(self.app.as_mut(), viewport_ui_cb.as_deref(), raw_input);

        // ------------------------------------------------------------

        let Self {
            integration,
            app,
            glutin,
            painter,
            ..
        } = self;

        let mut glutin = glutin.borrow_mut();
        let mut painter = painter.borrow_mut();

        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = full_output;

        let GlutinWindowContext {
            viewports,
            current_gl_context,
            ..
        } = &mut *glutin;

        let viewport = viewports.get_mut(&viewport_id).unwrap();
        let window = viewport.window.as_ref().unwrap();
        let gl_surface = viewport.gl_surface.as_ref().unwrap();
        let egui_winit = viewport.egui_winit.as_mut().unwrap();

        integration.post_update();
        egui_winit.handle_platform_output(window, &integration.egui_ctx, platform_output);

        let clipped_primitives = integration.egui_ctx.tessellate(shapes, pixels_per_point);

        {
            // TODO: only do this if we actually have multiple viewports
            crate::profile_scope!("change_gl_context");

            let not_current = {
                crate::profile_scope!("make_not_current");
                current_gl_context
                    .take()
                    .unwrap()
                    .make_not_current()
                    .unwrap()
            };

            crate::profile_scope!("make_current");
            *current_gl_context = Some(not_current.make_current(gl_surface).unwrap());
        }

        let screen_size_in_pixels: [u32; 2] = window.inner_size().into();

        painter.clear(
            screen_size_in_pixels,
            app.clear_color(&integration.egui_ctx.style().visuals),
        );

        painter.paint_and_update_textures(
            screen_size_in_pixels,
            pixels_per_point,
            &clipped_primitives,
            &textures_delta,
        );

        {
            let screenshot_requested = std::mem::take(&mut viewport.screenshot_requested);
            if screenshot_requested {
                let screenshot = painter.read_screen_rgba(screen_size_in_pixels);
                egui_winit
                    .egui_input_mut()
                    .events
                    .push(egui::Event::Screenshot {
                        viewport_id,
                        image: screenshot.into(),
                    });
            }
            integration.post_rendering(window);
        }

        {
            crate::profile_scope!("swap_buffers");
            if let Err(err) = gl_surface.swap_buffers(
                current_gl_context
                    .as_ref()
                    .expect("failed to get current context to swap buffers"),
            ) {
                log::error!("swap_buffers failed: {err}");
            }
        }

        // give it time to settle:
        #[cfg(feature = "__screenshot")]
        if integration.egui_ctx.frame_nr() == 2 {
            if let Ok(path) = std::env::var("EFRAME_SCREENSHOT_TO") {
                save_screeshot_and_exit(&path, &painter, screen_size_in_pixels);
            }
        }

        integration.maybe_autosave(app.as_mut(), Some(window));

        if window.is_minimized() == Some(true) {
            // On Mac, a minimized Window uses up all CPU:
            // https://github.com/emilk/egui/issues/325
            crate::profile_scope!("minimized_sleep");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        glutin.handle_viewport_output(&integration.egui_ctx, viewport_output);

        if integration.should_close() {
            EventResult::Exit
        } else {
            EventResult::Wait
        }
    }

    fn on_window_event(
        &mut self,
        window_id: WindowId,
        event: &winit::event::WindowEvent<'_>,
    ) -> EventResult {
        crate::profile_function!(egui_winit::short_window_event_description(event));

        let mut glutin = self.glutin.borrow_mut();
        let viewport_id = glutin.viewport_from_window.get(&window_id).copied();

        // On Windows, if a window is resized by the user, it should repaint synchronously, inside the
        // event handler.
        //
        // If this is not done, the compositor will assume that the window does not want to redraw,
        // and continue ahead.
        //
        // In eframe's case, that causes the window to rapidly flicker, as it struggles to deliver
        // new frames to the compositor in time.
        //
        // The flickering is technically glutin or glow's fault, but we should be responding properly
        // to resizes anyway, as doing so avoids dropping frames.
        //
        // See: https://github.com/emilk/egui/issues/903
        let mut repaint_asap = false;

        match event {
            winit::event::WindowEvent::Focused(new_focused) => {
                glutin.focused_viewport = new_focused.then(|| viewport_id).flatten();
            }

            winit::event::WindowEvent::Resized(physical_size) => {
                // Resize with 0 width and height is used by winit to signal a minimize event on Windows.
                // See: https://github.com/rust-windowing/winit/issues/208
                // This solves an issue where the app would panic when minimizing on Windows.
                if 0 < physical_size.width && 0 < physical_size.height {
                    if let Some(viewport_id) = viewport_id {
                        repaint_asap = true;
                        glutin.resize(viewport_id, *physical_size);
                    }
                }
            }

            winit::event::WindowEvent::ScaleFactorChanged { new_inner_size, .. } => {
                if let Some(viewport_id) = viewport_id {
                    repaint_asap = true;
                    glutin.resize(viewport_id, **new_inner_size);
                }
            }

            winit::event::WindowEvent::CloseRequested => {
                if viewport_id == Some(ViewportId::ROOT) && self.integration.should_close() {
                    log::debug!(
                        "Received WindowEvent::CloseRequested for main viewport - shutting down."
                    );
                    return EventResult::Exit;
                }

                log::debug!("Received WindowEvent::CloseRequested for viewport {viewport_id:?}");

                if let Some(viewport_id) = viewport_id {
                    if let Some(viewport) = glutin.viewports.get_mut(&viewport_id) {
                        // Tell viewport it should close:
                        viewport.info.events.push(egui::ViewportEvent::Close);

                        // We may need to repaint both us and our parent to close the window,
                        // and perhaps twice (once to notice the close-event, once again to enforce it).
                        // `request_repaint_of` does a double-repaint though:
                        self.integration.egui_ctx.request_repaint_of(viewport_id);
                        self.integration
                            .egui_ctx
                            .request_repaint_of(viewport.ids.parent);
                    }
                }
            }

            _ => {}
        }

        if self.integration.should_close() {
            return EventResult::Exit;
        }

        let mut event_response = EventResponse {
            consumed: false,
            repaint: false,
        };
        if let Some(viewport_id) = viewport_id {
            if let Some(viewport) = glutin.viewports.get_mut(&viewport_id) {
                event_response = self.integration.on_window_event(
                    self.app.as_mut(),
                    event,
                    viewport.egui_winit.as_mut().unwrap(),
                    viewport.ids.this,
                );
            }
        }

        if event_response.repaint {
            if repaint_asap {
                EventResult::RepaintNow(window_id)
            } else {
                EventResult::RepaintNext(window_id)
            }
        } else {
            EventResult::Wait
        }
    }
}

impl GlutinWindowContext {
    #[allow(unsafe_code)]
    unsafe fn new(
        egui_ctx: &egui::Context,
        viewport_builder: ViewportBuilder,
        native_options: &NativeOptions,
        event_loop: &EventLoopWindowTarget<UserEvent>,
    ) -> Result<Self> {
        crate::profile_function!();

        // There is a lot of complexity with opengl creation,
        // so prefer extensive logging to get all the help we can to debug issues.

        use glutin::prelude::*;
        // convert native options to glutin options
        let hardware_acceleration = match native_options.hardware_acceleration {
            crate::HardwareAcceleration::Required => Some(true),
            crate::HardwareAcceleration::Preferred => None,
            crate::HardwareAcceleration::Off => Some(false),
        };
        let swap_interval = if native_options.vsync {
            glutin::surface::SwapInterval::Wait(std::num::NonZeroU32::new(1).unwrap())
        } else {
            glutin::surface::SwapInterval::DontWait
        };
        /*  opengl setup flow goes like this:
            1. we create a configuration for opengl "Display" / "Config" creation
            2. choose between special extensions like glx or egl or wgl and use them to create config/display
            3. opengl context configuration
            4. opengl context creation
        */
        // start building config for gl display
        let config_template_builder = glutin::config::ConfigTemplateBuilder::new()
            .prefer_hardware_accelerated(hardware_acceleration)
            .with_depth_size(native_options.depth_buffer)
            .with_stencil_size(native_options.stencil_buffer)
            .with_transparency(native_options.viewport.transparent.unwrap_or(false));
        // we don't know if multi sampling option is set. so, check if its more than 0.
        let config_template_builder = if native_options.multisampling > 0 {
            config_template_builder.with_multisampling(
                native_options
                    .multisampling
                    .try_into()
                    .expect("failed to fit multisamples option of native_options into u8"),
            )
        } else {
            config_template_builder
        };

        log::debug!("trying to create glutin Display with config: {config_template_builder:?}");

        // Create GL display. This may probably create a window too on most platforms. Definitely on `MS windows`. Never on Android.
        let display_builder = glutin_winit::DisplayBuilder::new()
            // we might want to expose this option to users in the future. maybe using an env var or using native_options.
            .with_preference(glutin_winit::ApiPrefence::FallbackEgl) // https://github.com/emilk/egui/issues/2520#issuecomment-1367841150
            .with_window_builder(Some(create_winit_window_builder(
                egui_ctx,
                event_loop,
                viewport_builder.clone(),
            )));

        let (window, gl_config) = {
            crate::profile_scope!("DisplayBuilder::build");

            display_builder
                .build(
                    event_loop,
                    config_template_builder.clone(),
                    |mut config_iterator| {
                        let config = config_iterator.next().expect(
                            "failed to find a matching configuration for creating glutin config",
                        );
                        log::debug!(
                            "using the first config from config picker closure. config: {config:?}"
                        );
                        config
                    },
                )
                .map_err(|e| crate::Error::NoGlutinConfigs(config_template_builder.build(), e))?
        };
        if let Some(window) = &window {
            apply_viewport_builder_to_new_window(window, &viewport_builder);
        }

        let gl_display = gl_config.display();
        log::debug!(
            "successfully created GL Display with version: {} and supported features: {:?}",
            gl_display.version_string(),
            gl_display.supported_features()
        );
        let raw_window_handle = window.as_ref().map(|w| w.raw_window_handle());
        log::debug!("creating gl context using raw window handle: {raw_window_handle:?}");

        // create gl context. if core context cannot be created, try gl es context as fallback.
        let context_attributes =
            glutin::context::ContextAttributesBuilder::new().build(raw_window_handle);
        let fallback_context_attributes = glutin::context::ContextAttributesBuilder::new()
            .with_context_api(glutin::context::ContextApi::Gles(None))
            .build(raw_window_handle);

        let gl_context_result = unsafe {
            crate::profile_scope!("create_context");
            gl_config
                .display()
                .create_context(&gl_config, &context_attributes)
        };

        let gl_context = match gl_context_result {
            Ok(it) => it,
            Err(err) => {
                log::warn!("Failed to create context using default context attributes {context_attributes:?} due to error: {err}");
                log::debug!(
                    "Retrying with fallback context attributes: {fallback_context_attributes:?}"
                );
                unsafe {
                    gl_config
                        .display()
                        .create_context(&gl_config, &fallback_context_attributes)?
                }
            }
        };
        let not_current_gl_context = Some(gl_context);

        let mut viewport_from_window = HashMap::default();
        let mut window_from_viewport = ViewportIdMap::default();
        let mut info = ViewportInfo::default();
        if let Some(window) = &window {
            viewport_from_window.insert(window.id(), ViewportId::ROOT);
            window_from_viewport.insert(ViewportId::ROOT, window.id());
            info.minimized = window.is_minimized();
            info.maximized = Some(window.is_maximized());
        }

        let mut viewports = ViewportIdMap::default();
        viewports.insert(
            ViewportId::ROOT,
            Viewport {
                ids: ViewportIdPair::ROOT,
                class: ViewportClass::Root,
                builder: viewport_builder,
                info,
                screenshot_requested: false,
                viewport_ui_cb: None,
                gl_surface: None,
                window: window.map(Rc::new),
                egui_winit: None,
            },
        );

        // the fun part with opengl gl is that we never know whether there is an error. the context creation might have failed, but
        // it could keep working until we try to make surface current or swap buffers or something else. future glutin improvements might
        // help us start from scratch again if we fail context creation and go back to preferEgl or try with different config etc..
        // https://github.com/emilk/egui/pull/2541#issuecomment-1370767582

        let mut slf = GlutinWindowContext {
            egui_ctx: egui_ctx.clone(),
            swap_interval,
            gl_config,
            current_gl_context: None,
            not_current_gl_context,
            viewports,
            viewport_from_window,
            max_texture_side: None,
            window_from_viewport,
            focused_viewport: Some(ViewportId::ROOT),
        };

        slf.on_resume(event_loop)?;

        Ok(slf)
    }

    /// This will be run after `new`. on android, it might be called multiple times over the course of the app's lifetime.
    /// roughly,
    /// 1. check if window already exists. otherwise, create one now.
    /// 2. create attributes for surface creation.
    /// 3. create surface.
    /// 4. make surface and context current.
    ///
    /// we presently assume that we will
    fn on_resume(&mut self, event_loop: &EventLoopWindowTarget<UserEvent>) -> Result<()> {
        crate::profile_function!();

        let viewports: Vec<ViewportId> = self
            .viewports
            .iter()
            .filter(|(_, viewport)| viewport.gl_surface.is_none())
            .map(|(id, _)| *id)
            .collect();

        for viewport_id in viewports {
            self.init_viewport(viewport_id, event_loop)?;
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    pub(crate) fn init_viewport(
        &mut self,
        viewport_id: ViewportId,
        event_loop: &EventLoopWindowTarget<UserEvent>,
    ) -> Result<()> {
        crate::profile_function!();

        let viewport = self
            .viewports
            .get_mut(&viewport_id)
            .expect("viewport doesn't exist");

        let window = if let Some(window) = &mut viewport.window {
            window
        } else {
            log::trace!("Window doesn't exist yet. Creating one now with finalize_window");
            let window = glutin_winit::finalize_window(
                event_loop,
                create_winit_window_builder(&self.egui_ctx, event_loop, viewport.builder.clone()),
                &self.gl_config,
            )?;
            apply_viewport_builder_to_new_window(&window, &viewport.builder);
            viewport.info.minimized = window.is_minimized();
            viewport.info.maximized = Some(window.is_maximized());
            viewport.window.insert(Rc::new(window))
        };

        {
            // surface attributes
            let (width_px, height_px): (u32, u32) = window.inner_size().into();
            let width_px = std::num::NonZeroU32::new(width_px.at_least(1)).unwrap();
            let height_px = std::num::NonZeroU32::new(height_px.at_least(1)).unwrap();
            let surface_attributes =
                glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
                    .build(window.raw_window_handle(), width_px, height_px);

            log::trace!("creating surface with attributes: {surface_attributes:?}");
            let gl_surface = unsafe {
                self.gl_config
                    .display()
                    .create_window_surface(&self.gl_config, &surface_attributes)?
            };

            log::trace!("surface created successfully: {gl_surface:?}. making context current");

            let not_current_gl_context =
                if let Some(not_current_context) = self.not_current_gl_context.take() {
                    not_current_context
                } else {
                    self.current_gl_context
                        .take()
                        .unwrap()
                        .make_not_current()
                        .unwrap()
                };
            let current_gl_context = not_current_gl_context.make_current(&gl_surface)?;

            // try setting swap interval. but its not absolutely necessary, so don't panic on failure.
            log::trace!("made context current. setting swap interval for surface");
            if let Err(err) = gl_surface.set_swap_interval(&current_gl_context, self.swap_interval)
            {
                log::warn!("Failed to set swap interval due to error: {err}");
            }

            // we will reach this point only once in most platforms except android.
            // create window/surface/make context current once and just use them forever.

            viewport.egui_winit.get_or_insert_with(|| {
                egui_winit::State::new(
                    viewport_id,
                    event_loop,
                    Some(window.scale_factor() as f32),
                    self.max_texture_side,
                )
            });

            viewport.gl_surface = Some(gl_surface);
            self.current_gl_context = Some(current_gl_context);
            self.viewport_from_window
                .insert(window.id(), viewport.ids.this);
            self.window_from_viewport
                .insert(viewport.ids.this, window.id());
        }

        Ok(())
    }

    /// only applies for android. but we basically drop surface + window and make context not current
    fn on_suspend(&mut self) -> Result<()> {
        log::debug!("received suspend event. dropping window and surface");
        for viewport in self.viewports.values_mut() {
            viewport.gl_surface = None;
            viewport.window = None;
        }
        if let Some(current) = self.current_gl_context.take() {
            log::debug!("context is current, so making it non-current");
            self.not_current_gl_context = Some(current.make_not_current()?);
        } else {
            log::debug!("context is already not current??? could be duplicate suspend event");
        }
        Ok(())
    }

    fn viewport(&self, viewport_id: ViewportId) -> &Viewport {
        self.viewports
            .get(&viewport_id)
            .expect("viewport doesn't exist")
    }

    fn window(&self, viewport_id: ViewportId) -> Rc<Window> {
        self.viewport(viewport_id)
            .window
            .clone()
            .expect("winit window doesn't exist")
    }

    fn resize(&mut self, viewport_id: ViewportId, physical_size: winit::dpi::PhysicalSize<u32>) {
        let width_px = std::num::NonZeroU32::new(physical_size.width.at_least(1)).unwrap();
        let height_px = std::num::NonZeroU32::new(physical_size.height.at_least(1)).unwrap();

        if let Some(viewport) = self.viewports.get(&viewport_id) {
            if let Some(gl_surface) = &viewport.gl_surface {
                self.current_gl_context = Some(
                    self.current_gl_context
                        .take()
                        .unwrap()
                        .make_not_current()
                        .unwrap()
                        .make_current(gl_surface)
                        .unwrap(),
                );
                gl_surface.resize(
                    self.current_gl_context
                        .as_ref()
                        .expect("failed to get current context to resize surface"),
                    width_px,
                    height_px,
                );
            }
        }
    }

    fn get_proc_address(&self, addr: &std::ffi::CStr) -> *const std::ffi::c_void {
        self.gl_config.display().get_proc_address(addr)
    }

    fn handle_viewport_output(
        &mut self,
        egui_ctx: &egui::Context,
        viewport_output: ViewportIdMap<ViewportOutput>,
    ) {
        crate::profile_function!();

        let active_viewports_ids: ViewportIdSet = viewport_output.keys().copied().collect();

        for (
            viewport_id,
            ViewportOutput {
                parent,
                class,
                builder,
                viewport_ui_cb,
                commands,
                repaint_delay: _, // ignored - we listened to the repaint callback instead
            },
        ) in viewport_output
        {
            let ids = ViewportIdPair::from_self_and_parent(viewport_id, parent);

            let viewport = initialize_or_update_viewport(
                egui_ctx,
                &mut self.viewports,
                ids,
                class,
                builder,
                viewport_ui_cb,
                self.focused_viewport,
            );

            if let Some(window) = &viewport.window {
                let is_viewport_focused = self.focused_viewport == Some(viewport_id);
                egui_winit::process_viewport_commands(
                    egui_ctx,
                    &mut viewport.info,
                    commands,
                    window,
                    is_viewport_focused,
                    &mut viewport.screenshot_requested,
                );
            }
        }

        // GC old viewports
        self.viewports
            .retain(|id, _| active_viewports_ids.contains(id));
        self.viewport_from_window
            .retain(|_, id| active_viewports_ids.contains(id));
        self.window_from_viewport
            .retain(|id, _| active_viewports_ids.contains(id));
    }
}

impl Viewport {
    /// Update the stored `ViewportInfo`.
    fn update_viewport_info(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let Some(egui_winit) = &self.egui_winit else {
            return;
        };
        egui_winit.update_viewport_info(&mut self.info, window);
    }
}

fn initialize_or_update_viewport<'vp>(
    egu_ctx: &'_ egui::Context,
    viewports: &'vp mut ViewportIdMap<Viewport>,
    ids: ViewportIdPair,
    class: ViewportClass,
    mut builder: ViewportBuilder,
    viewport_ui_cb: Option<Arc<dyn Fn(&egui::Context) + Send + Sync>>,
    focused_viewport: Option<ViewportId>,
) -> &'vp mut Viewport {
    crate::profile_function!();

    if builder.icon.is_none() {
        // Inherit icon from parent
        builder.icon = viewports
            .get_mut(&ids.parent)
            .and_then(|vp| vp.builder.icon.clone());
    }

    match viewports.entry(ids.this) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            // New viewport:
            log::debug!("Creating new viewport {:?} ({:?})", ids.this, builder.title);
            entry.insert(Viewport {
                ids,
                class,
                builder,
                info: Default::default(),
                screenshot_requested: false,
                viewport_ui_cb,
                window: None,
                egui_winit: None,
                gl_surface: None,
            })
        }

        std::collections::hash_map::Entry::Occupied(mut entry) => {
            // Patch an existing viewport:
            let viewport = entry.get_mut();

            viewport.ids.parent = ids.parent;
            viewport.class = class;
            viewport.viewport_ui_cb = viewport_ui_cb;

            let (delta_commands, recreate) = viewport.builder.patch(builder);

            if recreate {
                log::debug!(
                    "Recreating window for viewport {:?} ({:?})",
                    ids.this,
                    viewport.builder.title
                );
                viewport.window = None;
                viewport.egui_winit = None;
            } else if let Some(window) = &viewport.window {
                let is_viewport_focused = focused_viewport == Some(ids.this);
                process_viewport_commands(
                    egu_ctx,
                    &mut viewport.info,
                    delta_commands,
                    window,
                    is_viewport_focused,
                    &mut viewport.screenshot_requested,
                );
            }

            entry.into_mut()
        }
    }
}

/// This is called (via a callback) by user code to render immediate viewports,
/// i.e. viewport that are directly nested inside a parent viewport.
fn render_immediate_viewport(
    event_loop: &EventLoopWindowTarget<UserEvent>,
    egui_ctx: &egui::Context,
    glutin: &RefCell<GlutinWindowContext>,
    painter: &RefCell<egui_glow::Painter>,
    beginning: Instant,
    immediate_viewport: ImmediateViewport<'_>,
) {
    crate::profile_function!();

    let ImmediateViewport {
        ids,
        builder,
        viewport_ui_cb,
    } = immediate_viewport;

    {
        let mut glutin = glutin.borrow_mut();

        let viewport = initialize_or_update_viewport(
            egui_ctx,
            &mut glutin.viewports,
            ids,
            ViewportClass::Immediate,
            builder,
            None,
            None,
        );

        if viewport.gl_surface.is_none() {
            glutin
                .init_viewport(ids.this, event_loop)
                .expect("Failed to initialize window in egui::Context::show_viewport_immediate");
        }
    }

    let input = {
        let mut glutin = glutin.borrow_mut();

        let Some(viewport) = glutin.viewports.get_mut(&ids.this) else {
            return;
        };
        viewport.update_viewport_info();
        let Some(winit_state) = &mut viewport.egui_winit else {
            return;
        };
        let Some(window) = &viewport.window else {
            return;
        };

        let mut raw_input = winit_state.take_egui_input(window);
        raw_input.viewports = glutin
            .viewports
            .iter()
            .map(|(id, viewport)| (*id, viewport.info.clone()))
            .collect();
        raw_input.time = Some(beginning.elapsed().as_secs_f64());
        raw_input
    };

    // ---------------------------------------------------
    // Call the user ui-code, which could re-entrantly call this function again!
    // No locks may be hold while calling this function.

    let egui::FullOutput {
        platform_output,
        textures_delta,
        shapes,
        pixels_per_point,
        viewport_output,
    } = egui_ctx.run(input, |ctx| {
        viewport_ui_cb(ctx);
    });

    // ---------------------------------------------------

    let mut glutin = glutin.borrow_mut();

    let GlutinWindowContext {
        current_gl_context,
        viewports,
        ..
    } = &mut *glutin;

    let Some(viewport) = viewports.get_mut(&ids.this) else {
        return;
    };

    let Some(winit_state) = &mut viewport.egui_winit else {
        return;
    };
    let (Some(window), Some(gl_surface)) = (&viewport.window, &viewport.gl_surface) else {
        return;
    };

    let screen_size_in_pixels: [u32; 2] = window.inner_size().into();

    let clipped_primitives = egui_ctx.tessellate(shapes, pixels_per_point);

    let mut painter = painter.borrow_mut();

    *current_gl_context = Some(
        current_gl_context
            .take()
            .unwrap()
            .make_not_current()
            .unwrap()
            .make_current(gl_surface)
            .unwrap(),
    );

    let current_gl_context = current_gl_context.as_ref().unwrap();

    if !gl_surface.is_current(current_gl_context) {
        log::error!("egui::show_viewport_immediate: viewport {:?}  ({:?}) is not created in main thread, try to use wgpu!", viewport.ids.this, viewport.builder.title);
    }

    let gl = &painter.gl().clone();
    egui_glow::painter::clear(gl, screen_size_in_pixels, [0.0, 0.0, 0.0, 0.0]);

    painter.paint_and_update_textures(
        screen_size_in_pixels,
        pixels_per_point,
        &clipped_primitives,
        &textures_delta,
    );

    {
        crate::profile_scope!("swap_buffers");
        if let Err(err) = gl_surface.swap_buffers(current_gl_context) {
            log::error!("swap_buffers failed: {err}");
        }
    }

    winit_state.handle_platform_output(window, egui_ctx, platform_output);

    glutin.handle_viewport_output(egui_ctx, viewport_output);
}

#[cfg(feature = "__screenshot")]
fn save_screeshot_and_exit(
    path: &str,
    painter: &egui_glow::Painter,
    screen_size_in_pixels: [u32; 2],
) {
    assert!(
        path.ends_with(".png"),
        "Expected EFRAME_SCREENSHOT_TO to end with '.png', got {path:?}"
    );
    let screenshot = painter.read_screen_rgba(screen_size_in_pixels);
    image::save_buffer(
        path,
        screenshot.as_raw(),
        screenshot.width() as u32,
        screenshot.height() as u32,
        image::ColorType::Rgba8,
    )
    .unwrap_or_else(|err| {
        panic!("Failed to save screenshot to {path:?}: {err}");
    });
    eprintln!("Screenshot saved to {path:?}.");

    #[allow(clippy::exit)]
    std::process::exit(0);
}