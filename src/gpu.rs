//! GPU-путь рендера видео (`MPV_RENDER_API_TYPE_OPENGL`).
//!
//! mpv рисует кадр прямо в OpenGL-текстуру (свой FBO), а egui показывает её
//! обычным квадом из paint-колбэка. По сравнению с программным путём исчезают
//! два прохода по каждому кадру: конвертация `rgb0 → Color32` и загрузка
//! мегабайтов в текстуру, а масштабирование делает видеокарта.
//!
//! Важно: GL-контекст окна гарантированно текущий только внутри
//! `egui::PaintCallback`, поэтому создание всех ресурсов и рендер живут там же.

use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};

use eframe::egui;
use eframe::egui_glow;
use eframe::glow;

use crate::libmpv::{MpvOpenGlInitParams, MpvPlayer};

const VERT: &str = r#"#version 330 core
layout (location = 0) in vec2 a_pos;
out vec2 v_pos;
void main() {
    // NDC -> «экранные» UV: (0,0) — левый верхний угол области.
    v_pos = vec2((a_pos.x + 1.0) * 0.5, 1.0 - (a_pos.y + 1.0) * 0.5);
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const FRAG: &str = r#"#version 330 core
in vec2 v_pos;
out vec4 f_color;
uniform sampler2D u_tex;
uniform vec2 u_size;
uniform float u_radius;
void main() {
    // Скруглённые углы: расстояние до прямоугольника (SDF), как у egui.
    vec2 half_size = u_size * 0.5;
    vec2 d = abs(v_pos * u_size - half_size) - (half_size - vec2(u_radius));
    float dist = length(max(d, vec2(0.0))) + min(max(d.x, d.y), 0.0) - u_radius;
    float a = smoothstep(1.0, 0.0, dist);
    if (a <= 0.0) {
        discard;
    }
    // Кадр mpv кладёт в FBO «сверху вниз» (строка 0 — верх картинки), как и в
    // эталонном примере `render_gl.c`, поэтому UV берём как есть. Программный
    // путь (`libmpv::rgb0_to_color_image`) тоже не переставляет строки.
    // Проверено скриншотами против кадра, который mpv пишет в PNG: с любым
    // переворотом картинка оказывается вверх ногами.
    vec2 uv = v_pos;
    vec3 c = texture(u_tex, uv).rgb;
    // egui рисует с premultiplied alpha — формат вывода тот же.
    f_color = vec4(c * a, a);
}
"#;

/// Указатель на плеер для paint-колбэка. Колбэк выполняется на UI-потоке, где
/// плеер и создан, поэтому гонок нет; `Send`/`Sync` нужны формально
/// (`egui_glow::CallbackFn` требует их от замыкания).
#[derive(Clone, Copy)]
pub struct PlayerPtr(pub *mut MpvPlayer);

unsafe impl Send for PlayerPtr {}
unsafe impl Sync for PlayerPtr {}

/// Общая часть состояния: UI читает `frames()`/`has_failed()`, колбэк пишет.
struct State {
    gl: Option<Arc<glow::Context>>,
    tex: Option<glow::Texture>,
    fbo: Option<glow::Framebuffer>,
    program: Option<glow::Program>,
    vao: Option<glow::VertexArray>,
    vbo: Option<glow::Buffer>,
    size: (u32, u32),
    /// GL-инициализация mpv выполнена.
    ready: bool,
    failed: bool,
    error: Option<String>,
    frames: u64,
}

// Состояние живёт в одном UI-потоке (колбэк egui тоже выполняется в нём),
// поэтому ручные impl'ы безопасны: доступ всегда под `Mutex`.
unsafe impl Send for State {}
unsafe impl Sync for State {}

/// Состояние GPU-рендера, разделяемое UI-потоком и paint-колбэком.
#[derive(Clone)]
pub struct GpuVideo {
    state: Arc<Mutex<State>>,
}

impl Default for GpuVideo {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuVideo {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                gl: None,
                tex: None,
                fbo: None,
                program: None,
                vao: None,
                vbo: None,
                size: (0, 0),
                ready: false,
                failed: false,
                error: None,
                frames: 0,
            })),
        }
    }

    /// Сколько кадров уже отрисовано через GPU (статистика/диагностика).
    #[allow(dead_code)]
    pub fn frames(&self) -> u64 {
        self.state.lock().map(|s| s.frames).unwrap_or(0)
    }

    /// GPU-путь не удался (нет GL 3.3, ошибка mpv) — нужно вернуться на ЦП.
    pub fn has_failed(&self) -> bool {
        self.state.lock().map(|s| s.failed).unwrap_or(false)
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.error.clone())
    }

    /// Сбрасывает привязку к плееру (после пересоздания mpv): GL-контекст mpv
    /// нужно создать заново, текстура и FBO переиспользуются.
    pub fn reset(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.ready = false;
            st.failed = false;
            st.error = None;
        }
    }

    /// GL-контекст mpv создан — можно грузить поток.
    pub fn ready(&self) -> bool {
        self.state.lock().map(|s| s.ready).unwrap_or(false)
    }

    /// Колбэк, который рисует кадр mpv прямо в текстуру egui.
    /// `render_frame = false` — просто перерисовываем уже готовый кадр
    /// (нового кадра у mpv нет), это в разы дешевле.
    pub fn callback(
        &self,
        rect: egui::Rect,
        player: PlayerPtr,
        size: (u32, u32),
        radius: f32,
        render_frame: bool,
    ) -> egui::PaintCallback {
        let state = self.state.clone();
        let size = (size.0.max(2), size.1.max(2));
        let cb = egui_glow::CallbackFn::new(move |info, painter| {
            let mut st = match state.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            if st.failed {
                return;
            }
            let gl = painter.gl().clone();
            if let Err(e) = draw(&mut st, &gl, player, size, radius, render_frame, &info) {
                crate::debug_log(&format!("gpu: ошибка кадра: {e}"));
                st.failed = true;
                st.error = Some(e);
            }
        });
        egui::PaintCallback {
            rect,
            callback: Arc::new(cb),
        }
    }

    /// Освобождает GL-ресурсы. Вызывать при текущем GL-контексте
    /// (`eframe::App::on_exit`).
    pub fn release(&self) {
        let mut st = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(gl) = st.gl.clone() {
            unsafe {
                use glow::HasContext as _;
                if let Some(tex) = st.tex.take() {
                    gl.delete_texture(tex);
                }
                if let Some(fbo) = st.fbo.take() {
                    gl.delete_framebuffer(fbo);
                }
                if let Some(p) = st.program.take() {
                    gl.delete_program(p);
                }
                if let Some(v) = st.vao.take() {
                    gl.delete_vertex_array(v);
                }
                if let Some(b) = st.vbo.take() {
                    gl.delete_buffer(b);
                }
            }
        }
        st.ready = false;
        st.size = (0, 0);
    }
}

/// Функция получения GL-указателей, которую запрашивает mpv.
/// `ctx` не используется: на Windows GL-функции берутся из `opengl32.dll`
/// (`wglGetProcAddress`), а он один на процесс.
unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    gl_proc(name) as *mut c_void
}

/// `wglGetProcAddress` (а для функций GL 1.1 — `GetProcAddress` из opengl32.dll).
#[cfg(windows)]
unsafe fn gl_proc(name: *const c_char) -> *const c_void {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    type WglGetProcAddress = unsafe extern "system" fn(*const u8) -> *const c_void;

    static OPGL: AtomicIsize = AtomicIsize::new(0);
    let mut module = OPGL.load(Ordering::Relaxed);
    if module == 0 {
        module = LoadLibraryA(b"opengl32.dll\0".as_ptr()) as isize;
        if module == 0 {
            return std::ptr::null();
        }
        OPGL.store(module, Ordering::Relaxed);
    }
    // mpv спрашивает функции только при инициализации рендера — не горячий путь.
    if let Some(wgl) = GetProcAddress(module as _, b"wglGetProcAddress\0".as_ptr()) {
        let wgl: WglGetProcAddress = std::mem::transmute(wgl);
        let p = wgl(name as *const u8);
        if !p.is_null() {
            return p;
        }
    }
    match GetProcAddress(module as _, name as *const u8) {
        Some(f) => f as usize as *const c_void,
        None => std::ptr::null(),
    }
}

/// Не-Windows заглушка (проект собирается только под Windows).
#[cfg(not(windows))]
unsafe fn gl_proc(_name: *const c_char) -> *const c_void {
    std::ptr::null()
}

/// Вся работа внутри paint-колбэка: инициализация, рендер кадра, показ.
fn draw(
    st: &mut State,
    gl: &Arc<glow::Context>,
    player: PlayerPtr,
    size: (u32, u32),
    radius: f32,
    render_frame: bool,
    info: &egui::PaintCallbackInfo,
) -> Result<(), String> {
    use glow::HasContext as _;

    if player.0.is_null() {
        return Ok(());
    }

    // 1) GL-контекст mpv создаём только здесь: контекст окна текущий.
    if !st.ready {
        let mut init = MpvOpenGlInitParams {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: Arc::as_ptr(gl) as *mut c_void,
        };
        match unsafe { (*player.0).init_opengl_render(&mut init) } {
            Ok(()) => {
                let info = unsafe {
                    format!(
                        "GL {}, {}, {}",
                        gl.get_parameter_string(glow::VERSION),
                        gl.get_parameter_string(glow::RENDERER),
                        gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
                    )
                };
                crate::debug_log(&format!("gpu: контекст mpv создан ({info})"));
            }
            Err(e) => {
                crate::debug_log(&format!("gpu: init не удался: {e}"));
                return Err(e);
            }
        }
        st.gl = Some(gl.clone());
        st.ready = true;
    }

    unsafe {
        // 2) Текстура + FBO под текущий размер области.
        if st.tex.is_none() {
            let tex = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            alloc_storage(gl, size);
            gl.bind_texture(glow::TEXTURE_2D, None);

            let fbo = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(tex);
                return Err(format!("FBO неполный: 0x{status:X}"));
            }
            st.tex = Some(tex);
            st.fbo = Some(fbo);
            st.size = size;
        } else if st.size != size {
            // Размер изменился — пересоздаём хранилище того же GL-имени:
            // FBO и TextureId остаются валидными, кадр не мигает.
            if let Some(tex) = st.tex {
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                alloc_storage(gl, size);
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            st.size = size;
        }

        let (Some(tex), Some(fbo)) = (st.tex, st.fbo) else {
            return Ok(());
        };

        // 3) Кадр mpv — прямо в FBO, без копий через RAM. Если нового кадра
        //    нет, mpv не трогаем: остаётся прошлый (перерисовка почти бесплатна).
        if render_frame {
            (*player.0).render_opengl(fbo.0.get(), size.0, size.1)?;
        }

        // 4) Показываем кадр квадом (со скруглением) поверх.
        draw_quad(st, gl, tex, radius, info);
    }

    st.frames = st.frames.wrapping_add(1);
    if render_frame && (st.frames == 1 || st.frames % 600 == 0) {
        crate::debug_log(&format!("gpu: кадров {}, размер {:?}", st.frames, st.size));
    }
    Ok(())
}

/// Выделяет хранилище RGBA8 для уже привязанной текстуры.
unsafe fn alloc_storage(gl: &glow::Context, size: (u32, u32)) {
    use glow::HasContext as _;
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        size.0 as i32,
        size.1 as i32,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        None,
    );
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
}

/// Показывает текстуру квадом на весь viewport колбэка (со скруглением).
unsafe fn draw_quad(
    st: &mut State,
    gl: &glow::Context,
    tex: glow::Texture,
    radius: f32,
    info: &egui::PaintCallbackInfo,
) {
    use glow::HasContext as _;

    let viewport = info.viewport_in_pixels();
    let clip = info.clip_rect_in_pixels();

    // mpv мог изменить состояние GL — выставляем своё явно.
    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    gl.viewport(
        viewport.left_px,
        viewport.from_bottom_px,
        viewport.width_px,
        viewport.height_px,
    );
    gl.enable(glow::SCISSOR_TEST);
    gl.scissor(
        clip.left_px,
        clip.from_bottom_px,
        clip.width_px,
        clip.height_px,
    );
    gl.disable(glow::CULL_FACE);
    gl.disable(glow::DEPTH_TEST);
    gl.enable(glow::BLEND);
    gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

    if st.program.is_none() {
        match (link_program(gl), gl.create_vertex_array(), gl.create_buffer()) {
            (Ok(program), Ok(vao), Ok(vbo)) => {
                gl.bind_vertex_array(Some(vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                // Треугольная полоса на весь viewport (в NDC).
                let verts: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
                let bytes = std::slice::from_raw_parts(
                    verts.as_ptr() as *const u8,
                    std::mem::size_of_val(&verts),
                );
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
                gl.bind_vertex_array(None);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
                st.program = Some(program);
                st.vao = Some(vao);
                st.vbo = Some(vbo);
            }
            _ => return,
        }
    }

    let Some(program) = st.program else {
        return;
    };
    gl.use_program(Some(program));
    gl.bind_vertex_array(st.vao);
    gl.active_texture(glow::TEXTURE0);
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    if let Some(loc) = gl.get_uniform_location(program, "u_tex") {
        gl.uniform_1_i32(Some(&loc), 0);
    }
    if let Some(loc) = gl.get_uniform_location(program, "u_size") {
        gl.uniform_2_f32(
            Some(&loc),
            viewport.width_px as f32,
            viewport.height_px as f32,
        );
    }
    if let Some(loc) = gl.get_uniform_location(program, "u_radius") {
        gl.uniform_1_f32(Some(&loc), radius);
    }
    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

    gl.bind_texture(glow::TEXTURE_2D, None);
    gl.bind_vertex_array(None);
    gl.disable(glow::SCISSOR_TEST);
}

/// Собирает простую программу «текстура на квад».
unsafe fn link_program(gl: &glow::Context) -> Result<glow::Program, String> {
    use glow::HasContext as _;
    let program = gl.create_program()?;
    let mut shaders = Vec::with_capacity(2);
    for (src, kind) in [(VERT, glow::VERTEX_SHADER), (FRAG, glow::FRAGMENT_SHADER)] {
        let shader = gl.create_shader(kind)?;
        gl.shader_source(shader, src);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            gl.delete_program(program);
            for s in shaders {
                gl.delete_shader(s);
            }
            return Err(format!("GLSL: {log}"));
        }
        gl.attach_shader(program, shader);
        shaders.push(shader);
    }
    gl.link_program(program);
    let ok = gl.get_program_link_status(program);
    for s in shaders {
        gl.detach_shader(program, s);
        gl.delete_shader(s);
    }
    if !ok {
        let log = gl.get_program_info_log(program);
        gl.delete_program(program);
        return Err(format!("GLSL link: {log}"));
    }
    Ok(program)
}
