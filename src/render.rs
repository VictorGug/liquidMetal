//! The entire renderer: one fullscreen triangle, one fragment shader.
//!
//! No SDL_Renderer, no vertex buffers, no textures. The vertex shader synthesises
//! its three positions from `gl_VertexID`, and every pixel of the blob is decided
//! by `shader.frag`.

use glow::HasContext;

use crate::physics::Ball;

/// Must match `uniform vec4 uBlobs[16]` in the fragment shader.
pub const MAX_BLOBS: usize = 16;

const VERT_SRC: &str = r#"#version 330 core
// Fullscreen triangle from the vertex index alone: (0,0), (2,0), (0,2) in UV,
// which is (-1,-1), (3,-1), (-1,3) in clip space and covers the viewport.
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAG_SRC: &str = include_str!("shader.frag");

pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    u_resolution: Option<glow::UniformLocation>,
    u_time: Option<glow::UniformLocation>,
    u_count: Option<glow::UniformLocation>,
    u_blobs: Option<glow::UniformLocation>,
    u_opaque: Option<glow::UniformLocation>,
    /// Scratch for the uniform upload, so the draw path does not allocate.
    scratch: [f32; MAX_BLOBS * 4],
    pub gl_version: String,
    pub gl_renderer: String,
    pub gl_vendor: String,
    pub glsl_version: String,
}

impl Renderer {
    /// `loader` resolves GL entry points; in practice `SDL_GL_GetProcAddress`.
    ///
    /// A GL context must already be current on this thread.
    pub fn new<F>(loader: F) -> Result<Renderer, String>
    where
        F: FnMut(&str) -> *const std::ffi::c_void,
    {
        // SAFETY: the caller guarantees a current GL context, which is the only
        // precondition `from_loader_function` and every `HasContext` call has.
        unsafe {
            let gl = glow::Context::from_loader_function(loader);

            let gl_version = gl.get_parameter_string(glow::VERSION);
            let gl_renderer = gl.get_parameter_string(glow::RENDERER);
            let gl_vendor = gl.get_parameter_string(glow::VENDOR);
            let glsl_version = gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION);

            let program = build_program(&gl, VERT_SRC, FRAG_SRC)?;

            // Core profile refuses to draw without a bound VAO, even when the draw
            // pulls no vertex attributes at all.
            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("could not create a vertex array object: {e}"))?;
            gl.bind_vertex_array(Some(vao));

            // Single pass writing final premultiplied values straight to the
            // framebuffer, so there is nothing to blend against.
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);

            let u_resolution = gl.get_uniform_location(program, "uResolution");
            let u_time = gl.get_uniform_location(program, "uTime");
            let u_count = gl.get_uniform_location(program, "uCount");
            let u_blobs = gl.get_uniform_location(program, "uBlobs");
            let u_opaque = gl.get_uniform_location(program, "uOpaque");
            for (name, loc) in [
                ("uResolution", &u_resolution),
                ("uTime", &u_time),
                ("uCount", &u_count),
                ("uBlobs", &u_blobs),
                ("uOpaque", &u_opaque),
            ] {
                if loc.is_none() {
                    // Not fatal — a driver may legitimately optimise one away — but
                    // a silently missing uniform is a black-screen bug waiting to
                    // happen, so it gets said out loud.
                    eprintln!("[gl] warning: uniform '{name}' was not found in the linked program");
                }
            }

            gl.use_program(Some(program));

            Ok(Renderer {
                gl,
                program,
                vao,
                u_resolution,
                u_time,
                u_count,
                u_blobs,
                u_opaque,
                scratch: [0.0; MAX_BLOBS * 4],
                gl_version,
                gl_renderer,
                gl_vendor,
                glsl_version,
            })
        }
    }

    /// Draw one frame.
    ///
    /// `scissor` is an optional `(x, y, w, h)` rectangle in top-down pixels; the
    /// clear always covers the whole surface, and only the shading is restricted.
    /// Ignored when `opaque`, which needs the background drawn everywhere.
    pub fn draw(
        &mut self,
        width: i32,
        height: i32,
        time: f32,
        balls: &[Ball],
        scissor: Option<(i32, i32, i32, i32)>,
        opaque: bool,
    ) {
        let count = balls.len().min(MAX_BLOBS);
        for (i, b) in balls.iter().take(count).enumerate() {
            self.scratch[i * 4] = b.p.x;
            self.scratch[i * 4 + 1] = b.p.y;
            self.scratch[i * 4 + 2] = b.r;
            self.scratch[i * 4 + 3] = b.w;
        }

        // SAFETY: context is current for the lifetime of `self`; every call below is
        // a plain state-setting or draw call with in-range arguments.
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width, height);

            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            if !opaque {
                if let Some((sx, sy, sw, sh)) = scissor {
                    if sw <= 0 || sh <= 0 {
                        return; // nothing visible this frame; the clear is the frame
                    }
                    // GL's scissor origin is bottom-left.
                    gl.scissor(sx, height - (sy + sh), sw, sh);
                    gl.enable(glow::SCISSOR_TEST);
                }
            }

            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.uniform_2_f32(self.u_resolution.as_ref(), width as f32, height as f32);
            gl.uniform_1_f32(self.u_time.as_ref(), time);
            gl.uniform_1_i32(self.u_count.as_ref(), count as i32);
            gl.uniform_1_i32(self.u_opaque.as_ref(), if opaque { 1 } else { 0 });
            gl.uniform_4_f32_slice(self.u_blobs.as_ref(), &self.scratch[..count * 4]);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            gl.disable(glow::SCISSOR_TEST);
        }
    }

    /// Read a rectangle of the framebuffer back as RGBA8, in top-down row order.
    ///
    /// Exists so the renderer can be checked without anyone being able to look at
    /// the screen: `--capture` writes this to a file, alpha channel intact.
    pub fn read_pixels(&self, x: i32, y: i32, w: i32, h: i32, fb_height: i32) -> Vec<u8> {
        let mut buf = vec![0u8; (w.max(0) * h.max(0) * 4) as usize];
        if buf.is_empty() {
            return buf;
        }
        // SAFETY: context is current; the buffer is exactly w*h*4 bytes, which is
        // what GL_RGBA/GL_UNSIGNED_BYTE writes for this rectangle.
        unsafe {
            self.gl.read_pixels(
                x,
                fb_height - (y + h), // GL reads bottom-up
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
        }
        // Flip to top-down so the file matches screen coordinates.
        let stride = (w * 4) as usize;
        let mut out = vec![0u8; buf.len()];
        for row in 0..h as usize {
            let src = (h as usize - 1 - row) * stride;
            out[row * stride..(row + 1) * stride].copy_from_slice(&buf[src..src + stride]);
        }
        out
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: same context, still current during teardown.
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
        }
    }
}

/// Compile and link, reporting the driver's log in full on any failure. A shader
/// that fails silently is a black screen with no clue, which is the worst possible
/// outcome to debug.
unsafe fn build_program(
    gl: &glow::Context,
    vert: &str,
    frag: &str,
) -> Result<glow::Program, String> {
    unsafe {
        let vs = compile(gl, glow::VERTEX_SHADER, vert, "vertex")?;
        let fs = match compile(gl, glow::FRAGMENT_SHADER, frag, "fragment") {
            Ok(s) => s,
            Err(e) => {
                gl.delete_shader(vs);
                return Err(e);
            }
        };

        let program = gl
            .create_program()
            .map_err(|e| format!("could not create a GL program: {e}"))?;
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        let ok = gl.get_program_link_status(program);
        let log = gl.get_program_info_log(program);
        gl.delete_shader(vs);
        gl.delete_shader(fs);
        if !ok {
            gl.delete_program(program);
            return Err(format!("shader link failed:\n{log}"));
        }
        if !log.trim().is_empty() {
            eprintln!("[gl] link log:\n{}", log.trim());
        }
        Ok(program)
    }
}

unsafe fn compile(
    gl: &glow::Context,
    kind: u32,
    src: &str,
    label: &str,
) -> Result<glow::Shader, String> {
    unsafe {
        let sh = gl
            .create_shader(kind)
            .map_err(|e| format!("could not create the {label} shader: {e}"))?;
        gl.shader_source(sh, src);
        gl.compile_shader(sh);
        if !gl.get_shader_compile_status(sh) {
            let log = gl.get_shader_info_log(sh);
            gl.delete_shader(sh);
            return Err(format!(
                "{label} shader failed to compile:\n{}\n--- source ---\n{}",
                log.trim(),
                numbered(src)
            ));
        }
        let log = gl.get_shader_info_log(sh);
        if !log.trim().is_empty() {
            eprintln!("[gl] {label} shader log:\n{}", log.trim());
        }
        Ok(sh)
    }
}

/// Line-numbered source, so a driver log saying "0:117" is immediately actionable.
fn numbered(src: &str) -> String {
    src.lines()
        .enumerate()
        .map(|(i, l)| format!("{:>4} | {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}
