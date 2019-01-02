use crate::gfx::{GfxBufferMutLE, Rgb888};
use crate::hw::glutils::Texture;
use crate::snd::{SampleFormat, SndBufferMut};

use imgui::*;
use imgui_opengl_renderer::Renderer;
use imgui_sdl2::ImguiSdl2;
use imgui_sys::{igSetNextWindowSizeConstraints, ImGuiSizeCallbackData};
use sdl2::keyboard::Scancode;
mod uisupport;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

// Views
mod regview;
pub use self::regview::*;
mod disasmview;
pub use self::disasmview::*;
mod decoding;
pub use self::decoding::*;
mod tracer;
pub use self::tracer::*;
mod uictx;
pub(crate) use self::uictx::*;
mod miscview;
pub(crate) use self::miscview::*;

pub trait DebuggerModel {
    /// Return a vector of the name of all CPUS.
    /// TODO: the debugger could autodiscover the CPUs while rendering.
    fn all_cpus(&self) -> Vec<String>;

    // Return the total elapsed cycles since the beginning of emulation
    fn cycles(&self) -> i64;

    // Return the number of emulated frames since the beginning of emulation
    fn frames(&self) -> i64;

    /// Run a frame with a tracer (debugger).
    ///
    /// The function is expected to respect trace API and call the trait methods at
    /// the correct moments, propagating any error (TraceEvent) generated by them
    /// Failure to do so may impede correct debugger functionality (eg: not calling
    /// trace_cpu() at every emulated frame may cause a breakpoint to be missed.
    ///
    /// After a TraceEvent is returned and processed by the debugger, emulation of the
    /// frame will be resumed by calling run_frame with the same screen buffer.
    fn trace_frame<SF: SampleFormat>(
        &mut self,
        screen: &mut GfxBufferMutLE<Rgb888>,
        sound: &mut SndBufferMut<SF>,
        tracer: &Tracer,
    ) -> Result<()>;

    /// Run a single CPU step with a tracer (debugger).
    /// Similar to trace_frame(), but blocks after the specified CPU has performed a single
    /// step (opcode).
    fn trace_step(&mut self, cpu_name: &str, tracer: &Tracer) -> Result<()>;

    /// Reset the emulator.
    fn reset(&mut self, hard: bool);

    fn render_debug<'a, 'ui>(&mut self, dr: &DebuggerRenderer<'a, 'ui>);
}

pub struct DebuggerUI {
    imgui: Rc<RefCell<ImGui>>,
    imgui_sdl2: ImguiSdl2,
    backend: Renderer,
    hidpi_factor: f32,
    tex_screen: Texture,
    screen_size: (usize, usize),

    pub dbg: Debugger,
    uictx: RefCell<UiCtx>,

    paused: bool,
    last_render: Instant, // last instant the debugger refreshed its UI
}

impl DebuggerUI {
    pub(crate) fn new<T: DebuggerModel>(video: sdl2::VideoSubsystem, producer: &mut T) -> Self {
        let hidpi_factor = 1.0;

        let mut imgui = ImGui::init();
        imgui.set_ini_filename(Some(im_str!("debug.ini").to_owned()));

        let imgui_sdl2 = ImguiSdl2::new(&mut imgui);
        let backend = Renderer::new(&mut imgui, move |s| video.gl_get_proc_address(s) as _);

        let mut uictx = UiCtx::default();
        uictx.cpus = producer.all_cpus();
        for idx in 0..uictx.cpus.len() {
            let name = &uictx.cpus[idx];
            uictx.disasm.insert(name.clone(), UiCtxDisasm::default());
        }

        // Initial event
        uictx.event = Some((box TraceEvent::Paused(), Instant::now()));

        Self {
            imgui: Rc::new(RefCell::new(imgui)),
            imgui_sdl2,
            backend,
            hidpi_factor,
            tex_screen: Texture::new(),
            screen_size: (320, 240),
            dbg: Debugger::new(&uictx.cpus),
            uictx: RefCell::new(uictx),
            paused: true,
            last_render: Instant::now(),
        }
    }

    pub(crate) fn handle_event(&mut self, event: &sdl2::event::Event) {
        let imgui = self.imgui.clone();
        let mut imgui = imgui.borrow_mut();
        self.imgui_sdl2.handle_event(&mut imgui, &event);
    }

    /// Run an emulator (DebuggerModel) under the debugger for a little while.
    /// Returns true if during this call the emulator completed a frame, or false otherwise.
    pub(crate) fn trace<T: DebuggerModel, SF: SampleFormat>(
        &mut self,
        producer: &mut T,
        screen: &mut GfxBufferMutLE<Rgb888>,
        sound: &mut SndBufferMut<SF>,
    ) -> bool {
        // If the emulation core is paused, we can simply wait here to avoid hogging CPU.
        // Refresh every 16ms / 60FPS.
        if self.paused {
            match Duration::from_millis(16).checked_sub(self.last_render.elapsed()) {
                Some(d) => std::thread::sleep(d),
                None => {}
            }
            return false;
        }

        // Request a Poll event after 50ms to keep the debugger at least at 20 FPS during emulation.
        let trace_until = self.last_render + Duration::from_millis(50);
        self.dbg.set_poll_event(trace_until);

        match producer.trace_frame(screen, sound, &self.dbg.new_tracer()) {
            Ok(()) => {
                // A frame is finished. Copy it into the texture so that it's available
                // starting from next render().
                self.tex_screen.copy_from_buffer_mut(screen);
                self.screen_size = (screen.width(), screen.height());
                return true;
            }
            Err(event) => {
                self.uictx.get_mut().event = Some((event.clone(), Instant::now()));
                match *event {
                    TraceEvent::Poll() => return false, // Polling
                    TraceEvent::Breakpoint(_, _, _) => {
                        self.paused = true;
                        self.dbg.disable_breakpoint_oneshot();
                        return false;
                    }
                    TraceEvent::WatchpointRead(cpu_name, _) => {
                        self.paused = true;
                        self.dbg.disable_breakpoint_oneshot();
                        self.uictx
                            .get_mut()
                            .add_flash_msg(&format!("Watchpoint (read) hit on {}", cpu_name));
                        return false;
                    }
                    TraceEvent::WatchpointWrite(cpu_name, _) => {
                        self.paused = true;
                        self.dbg.disable_breakpoint_oneshot();
                        self.uictx
                            .get_mut()
                            .add_flash_msg(&format!("Watchpoint (write) hit on {}", cpu_name));
                        return false;
                    }
                    TraceEvent::BreakpointOneShot(_, _) => {
                        self.paused = true;
                        self.dbg.disable_breakpoint_oneshot();
                        return false;
                    }
                    TraceEvent::GenericBreak(msg) => {
                        self.paused = true;
                        self.dbg.disable_breakpoint_oneshot();
                        self.uictx
                            .get_mut()
                            .add_flash_msg(&format!("Emulation stopped:\n{}", msg));
                        return false;
                    }
                    _ => unimplemented!(),
                }
            }
        };
    }

    /// Render the current debugger UI.
    pub(crate) fn render<T: DebuggerModel>(
        &mut self,
        window: &sdl2::video::Window,
        event_pump: &sdl2::EventPump,
        model: &mut T,
    ) {
        let imgui = self.imgui.clone();
        let mut imgui = imgui.borrow_mut();
        let ui = self.imgui_sdl2.frame(&window, &mut imgui, &event_pump);

        self.render_main(&ui, model);
        ui.show_demo_window(&mut true);

        {
            let dr = DebuggerRenderer {
                ui: &ui,
                ctx: &self.uictx,
            };
            model.render_debug(&dr);
        }

        // Actually flush commands batched in imgui to OpenGL
        unsafe {
            gl::ClearColor(0.45, 0.55, 0.60, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }

        self.backend.render(ui);
        self.last_render = Instant::now();

        let uictx = self.uictx.get_mut();
        uictx.event = None;
        match uictx.command {
            Some(UiCommand::Pause(paused)) => self.paused = paused,
            Some(UiCommand::BreakpointOneShot(ref cpu_name, pc)) => {
                let cpu_name = cpu_name.clone();
                self.dbg.set_breakpoint_oneshot(&cpu_name, Some(pc));
                self.paused = false;
            }
            Some(UiCommand::CpuStep(ref cpu_name)) => {
                let _ = model.trace_step(&cpu_name, &Tracer::null());
                self.paused = true;
                uictx.event = Some((box TraceEvent::Stepped(), Instant::now()));
            }
            None => {}
        };
        uictx.command = None;
    }

    fn render_main<'ui, T: DebuggerModel>(&mut self, ui: &Ui<'ui>, model: &mut T) {
        if ui.imgui().is_key_pressed(Scancode::Space as _) {
            self.paused = !self.paused;
            if self.paused {
                self.uictx.get_mut().event = Some((box TraceEvent::Paused(), Instant::now()));
            }
        }

        render_flash_msgs(ui, self.uictx.get_mut());

        let help = render_help(ui);
        if ui.imgui().is_key_pressed(Scancode::H as _) {
            ui.open_popup(&help);
        }

        ui.main_menu_bar(|| {
            ui.menu(im_str!("Emulation")).build(|| {
                if ui.menu_item(im_str!("Soft Reset")).build() {
                    model.reset(false);
                }
                if ui.menu_item(im_str!("Hard Reset")).build() {
                    model.reset(true);
                }
            });

            ui.same_line(200.0);
            ui.text(im_str!("State:"));
            if self.paused {
                ui.text(im_str!("PAUSED"));
                if ui.button(im_str!("Run"), (40.0, 20.0)) {
                    self.paused = false;
                }
            } else {
                ui.text(im_str!("RUNNING"));
                if ui.button(im_str!("Pause"), (40.0, 20.0)) {
                    self.paused = true;
                    self.uictx.get_mut().event = Some((box TraceEvent::Paused(), Instant::now()));
                }
            }

            ui.same_line(400.0);
            ui.text(format!(
                "Cycles: {}, Frames: {}",
                model.cycles(),
                model.frames()
            ));
        });

        unsafe {
            // Set constraint to avoid distortion of the screen window
            igSetNextWindowSizeConstraints(
                (100.0, 100.0).into(),
                (10000.0, 10000.0).into(),
                Some(screen_resize_callback),
                (&mut self.screen_size as *mut (usize, usize)) as *mut ::std::ffi::c_void,
            );
        }
        ui.window(im_str!("Screen"))
            .size((320.0, 240.0), ImGuiCond::FirstUseEver)
            .build(|| {
                let tsid = self.tex_screen.id();
                let reg = ui.get_content_region_avail();
                let image = Image::new(ui, tsid.into(), reg);
                image.build();
            });

        self.dbg.render_main(ui, self.uictx.get_mut());
    }
}

extern "C" fn screen_resize_callback(data: *mut ImGuiSizeCallbackData) {
    unsafe {
        // Constraint the screen window to the ratio of the actual framebuffer
        // (as stored in the last frame).
        let screen_size = (*data).user_data as *mut (usize, usize);
        let ratio = ((*screen_size).1 as f32) / ((*screen_size).0 as f32);
        (*data).desired_size.y = (*data).desired_size.x * ratio;
    }
}

pub struct DebuggerRenderer<'a, 'ui> {
    ui: &'a Ui<'ui>,
    ctx: &'a RefCell<UiCtx>,
}

impl<'a, 'ui> DebuggerRenderer<'a, 'ui> {
    pub fn render_regview<V: RegisterView>(&self, v: &mut V) {
        render_regview(self.ui, &mut self.ctx.borrow_mut(), v)
    }
    pub fn render_disasmview<V: DisasmView>(&self, v: &mut V) {
        render_disasmview(self.ui, &mut self.ctx.borrow_mut(), v)
    }
}
