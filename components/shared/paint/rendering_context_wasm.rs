//! Worker rendering context.
//!
//! Cloudflare Workers have no native display, GL context, or window handle.
//! This facade keeps Servo's rendering-context API available to the DOM/layout
//! pipeline while storing the rendered image in wasm-owned RGBA memory. The
//! WebRender-to-pixel integration will fill this buffer in a later step.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use embedder_traits::RefreshDriver;
use euclid::Size2D;
use euclid::default::Size2D as UntypedSize2D;
use gleam::gl::Gl;
use image::RgbaImage;
use webrender_api::units::{DeviceIntRect, DevicePixel};

/// Worker-side rendering failures are reported without depending on native GL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
}

/// Placeholder for native Surfman connections on a Worker.
#[derive(Clone, Debug, Default)]
pub struct Connection;

/// Placeholder for native Surfman adapters on a Worker.
#[derive(Clone, Debug, Default)]
pub struct Adapter;

/// Cloudflare Workers never expose a WebGL context, so the `glow` crate --
/// which unconditionally pulls in `wasm-bindgen`/`web_sys` for any
/// `wasm32` target regardless of which of its features are enabled -- must
/// not be a dependency of this target at all. This zero-sized stand-in
/// keeps `RenderingContext`'s API shape identical to the native
/// implementation (`rendering_context.rs`) without linking `glow`. Nothing
/// on this target ever constructs one; see `glow_gl_api` below.
#[derive(Clone, Debug, Default)]
pub struct NullGlContext;

/// Placeholder surface types retained for the shared paint API.
#[derive(Clone, Debug, Default)]
pub struct Surface;

#[derive(Clone, Debug, Default)]
pub struct SurfaceTexture;

impl Connection {
    pub fn create_adapter(&self) -> Result<Adapter, Error> {
        Err(Error::Unsupported)
    }
}

/// The rendering-context contract used by Servo's embedder and paint layers.
pub trait RenderingContext {
    fn prepare_for_rendering(&self) {}

    fn read_to_image(&self, _source_rectangle: DeviceIntRect) -> Option<RgbaImage>;

    fn size(&self) -> PhysicalSize<u32>;

    fn size2d(&self) -> Size2D<u32, DevicePixel> {
        let size = self.size();
        Size2D::new(size.width, size.height)
    }

    fn resize(&self, size: PhysicalSize<u32>);

    fn present(&self) {}

    fn make_current(&self) -> Result<(), Error> {
        Ok(())
    }

    fn gleam_gl_api(&self) -> Rc<dyn Gl>;

    fn glow_gl_api(&self) -> Arc<NullGlContext>;

    fn create_texture(
        &self,
        _surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        None
    }

    fn destroy_texture(&self, _surface_texture: SurfaceTexture) -> Option<Surface> {
        None
    }

    fn connection(&self) -> Option<Connection> {
        None
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        None
    }
}

/// A Worker-owned RGBA framebuffer suitable for screenshot extraction.
pub struct WorkerRenderingContext {
    size: RefCell<PhysicalSize<u32>>,
    pixels: RefCell<RgbaImage>,
    gleam: Option<Rc<dyn Gl>>,
    glow: Option<Arc<NullGlContext>>,
}

impl WorkerRenderingContext {
    pub fn new(size: PhysicalSize<u32>) -> Result<Self, Error> {
        if size.width == 0 || size.height == 0 {
            return Err(Error::Unsupported);
        }

        // Cloudflare Workers do not provide a native WebGL context. Servo's
        // DOM/event pipeline still needs a context object, though, so use a
        // null-loader facade until the software pixel backend is connected.
        // Rendering calls that require real GL remain unsupported; page
        // loading and JavaScript/DOM evaluation do not require them.
        let gleam = unsafe { gleam::gl::GlFns::load_with(|_| std::ptr::null()) };
        Ok(Self {
            size: RefCell::new(size),
            pixels: RefCell::new(RgbaImage::new(size.width, size.height)),
            gleam: Some(gleam),
            glow: None,
        })
    }

    pub fn pixels(&self) -> RgbaImage {
        self.pixels.borrow().clone()
    }
}

impl RenderingContext for WorkerRenderingContext {
    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        let pixels = self.pixels.borrow();
        let bounds = euclid::Box2D::new(
            euclid::Point2D::new(0, 0),
            euclid::Point2D::new(pixels.width() as i32, pixels.height() as i32),
        );
        let Some(clipped) = source_rectangle.intersection(&bounds) else {
            return None;
        };
        if clipped.is_empty() {
            return None;
        }
        Some(pixels.clone())
    }

    fn size(&self) -> PhysicalSize<u32> {
        *self.size.borrow()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        *self.size.borrow_mut() = size;
        *self.pixels.borrow_mut() = RgbaImage::new(size.width, size.height);
    }

    fn gleam_gl_api(&self) -> Rc<dyn Gl> {
        self.gleam.clone().expect("Worker has no GL context")
    }

    fn glow_gl_api(&self) -> Arc<NullGlContext> {
        self.glow.clone().expect("Worker has no GL context")
    }
}

pub type SoftwareRenderingContext = WorkerRenderingContext;
pub type WindowRenderingContext = WorkerRenderingContext;
pub type OffscreenRenderingContext = WorkerRenderingContext;
