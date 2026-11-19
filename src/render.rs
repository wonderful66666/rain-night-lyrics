use crate::desktop::Cover;
use std::f32::consts::TAU;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{
    ULW_ALPHA, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindowIndirect,
};
use windows::core::{Error, Result, w};

pub const CANVAS: (i32, i32) = (296, 120);
const LEFT: f32 = 40.0;
const TOP: f32 = 24.0;
const CX: f32 = LEFT + 22.0;
const CY: f32 = TOP + 22.0;

pub struct Frame<'a> {
    pub width: f32,
    pub opacity: f32,
    pub dx: f32,
    pub dy: f32,
    pub scale: (f32, f32),
    pub angle: f32,
    pub progress: Option<f32>,
    pub playing: bool,
    pub has_media: bool,
    pub cover: Option<&'a Cover>,
    pub status: &'a str,
    pub hover: bool,
    pub close_hover: bool,
    pub content: f32,
    pub tooltip: f32,
    pub breath: f32,
    pub ripple: f32,
    pub splash: f32,
}

impl Frame<'_> {
    fn record_only(&self) -> bool {
        self.width == 44.
            && self.opacity == 1.
            && self.dx == 0.
            && self.dy == 0.
            && self.scale == (1., 1.)
            && self.content <= 0.
            && self.tooltip <= 0.
            && self.ripple < 0.
            && self.splash < 0.
    }
}

fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}
fn ellipse(x: f32, y: f32, rx: f32, ry: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE {
        point: pt(x, y),
        radiusX: rx,
        radiusY: ry,
    }
}
fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}
fn pill(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect,
        radiusX: radius,
        radiusY: radius,
    }
}
fn color(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 255) as f32 / 255.0,
        g: ((rgb >> 8) & 255) as f32 / 255.0,
        b: (rgb & 255) as f32 / 255.0,
        a: a.clamp(0.0, 1.0),
    }
}
fn transform(sx: f32, sy: f32, tx: f32, ty: f32) -> Matrix3x2 {
    Matrix3x2 {
        M11: sx,
        M12: 0.,
        M21: 0.,
        M22: sy,
        M31: tx,
        M32: ty,
    }
}
fn disc_rotation(angle: f32) -> Matrix3x2 {
    let (sin, cos) = angle.to_radians().sin_cos();
    Matrix3x2 {
        M11: cos,
        M12: sin,
        M21: -sin,
        M22: cos,
        M31: CX - 17. * cos + 17. * sin,
        M32: CY - 17. * sin - 17. * cos,
    }
}
fn pixel_format() -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    }
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    size: SIZE,
}
impl Surface {
    fn new(dpi: u32) -> Result<Self> {
        let px = |value: i32| ((value as f64 * f64::from(dpi) / 96.).round() as i32).max(1);
        let size = SIZE {
            cx: px(CANVAS.0),
            cy: px(CANVAS.1),
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.cx,
                biHeight: -size.cy,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(Error::from_win32());
            }
            let bitmap = match CreateDIBSection(
                dc,
                &info,
                DIB_RGB_COLORS,
                &mut std::ptr::null_mut(),
                None,
                0,
            ) {
                Ok(bmp) => bmp,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let old = SelectObject(dc, bitmap);
            if old.is_invalid() {
                let err = Error::from_win32();
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(dc);
                return Err(err);
            }
            Ok(Self {
                dc,
                bitmap,
                old,
                size,
            })
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

fn stops(
    target: &ID2D1DCRenderTarget,
    colors: &[(f32, u32, f32)],
) -> Result<ID2D1GradientStopCollection> {
    let values: Vec<_> = colors
        .iter()
        .map(|&(position, rgb, alpha)| D2D1_GRADIENT_STOP {
            position,
            color: color(rgb, alpha),
        })
        .collect();
    unsafe { target.CreateGradientStopCollection(&values, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) }
}
fn linear(
    target: &ID2D1DCRenderTarget,
    start: D2D_POINT_2F,
    end: D2D_POINT_2F,
    colors: &[(f32, u32, f32)],
) -> Result<ID2D1LinearGradientBrush> {
    unsafe {
        target.CreateLinearGradientBrush(
            &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: start,
                endPoint: end,
            },
            None,
            &stops(target, colors)?,
        )
    }
}
fn make_path(
    factory: &ID2D1Factory,
    make: impl FnOnce(&ID2D1GeometrySink),
) -> Result<ID2D1PathGeometry> {
    unsafe {
        let path = factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        make(&sink);
        sink.Close()?;
        Ok(path)
    }
}
fn arc_end(x: f32, y: f32, rx: f32, ry: f32, large: bool, clockwise: bool) -> D2D1_ARC_SEGMENT {
    D2D1_ARC_SEGMENT {
        point: pt(x, y),
        size: D2D_SIZE_F {
            width: rx,
            height: ry,
        },
        rotationAngle: 0.,
        sweepDirection: if clockwise {
            D2D1_SWEEP_DIRECTION_CLOCKWISE
        } else {
            D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
        },
        arcSize: if large {
            D2D1_ARC_SIZE_LARGE
        } else {
            D2D1_ARC_SIZE_SMALL
        },
    }
}

struct Text {
    factory: IDWriteFactory,
    main: IDWriteTextLayout,
    button: IDWriteTextLayout,
    hint: IDWriteTextLayout,
    status_format: IDWriteTextFormat,
    status: Option<(String, IDWriteTextLayout, f32)>,
}
impl Text {
    fn new() -> Result<Self> {
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut fonts = None;
            factory.GetSystemFontCollection(&mut fonts, false)?;
            let mut found = BOOL(0);
            if let Some(fonts) = fonts {
                fonts.FindFamilyName(w!("Microsoft YaHei UI"), &mut 0, &mut found)?;
            }
            let family = if found.as_bool() {
                w!("Microsoft YaHei UI")
            } else {
                w!("Segoe UI")
            };
            let format = |size, weight| -> Result<IDWriteTextFormat> {
                let f = factory.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("zh-CN"),
                )?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                Ok(f)
            };
            let main_format = format(13., DWRITE_FONT_WEIGHT_MEDIUM)?;
            let hint_format = format(12., DWRITE_FONT_WEIGHT_NORMAL)?;
            hint_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let layout = |s: &str, f: &IDWriteTextFormat, w, h| {
                factory.CreateTextLayout(&s.encode_utf16().collect::<Vec<_>>(), f, w, h)
            };
            Ok(Self {
                main: layout("取词器运行中", &main_format, 108., 19.)?,
                button: layout("关闭", &main_format, 34., 34.)?,
                hint: layout("点一下，可关闭", &hint_format, 120., 28.)?,
                status_format: format(11., DWRITE_FONT_WEIGHT_NORMAL)?,
                status: None,
                factory,
            })
        }
    }
    fn update(&mut self, status: &str) -> Result<()> {
        if self
            .status
            .as_ref()
            .is_some_and(|(last, ..)| last == status)
        {
            return Ok(());
        }
        unsafe {
            let layout = self.factory.CreateTextLayout(
                &status.encode_utf16().collect::<Vec<_>>(),
                &self.status_format,
                250.,
                16.,
            )?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            self.status = Some((status.into(), layout, metrics.width));
        }
        Ok(())
    }
}

pub struct Renderer {
    factory: ID2D1Factory,
    target: ID2D1DCRenderTarget,
    ink: ID2D1SolidColorBrush,
    accent: ID2D1LinearGradientBrush,
    fallback: ID2D1LinearGradientBrush,
    highlight: ID2D1LinearGradientBrush,
    shadow: ID2D1RadialGradientBrush,
    round: ID2D1StrokeStyle,
    drop_icon: ID2D1PathGeometry,
    power: ID2D1PathGeometry,
    art: Option<(Vec<u8>, ID2D1BitmapBrush)>,
    progress: Option<(u16, ID2D1PathGeometry)>,
    previous_record_only: bool,
    bounds: RECT,
    text: Text,
    surface: Option<(u32, Surface)>,
}
impl Renderer {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let target = factory.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: pixel_format(),
                dpiX: 96.,
                dpiY: 96.,
                ..Default::default()
            })?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let ink = target.CreateSolidColorBrush(&color(0, 1.), None)?;
            let accent = linear(
                &target,
                pt(CX - 15., CY - 15.),
                pt(CX + 15., CY + 15.),
                &[(0., 0xc58bea, 1.), (1., 0xe58a9e, 1.)],
            )?;
            let fallback = linear(
                &target,
                pt(3., 3.),
                pt(31., 31.),
                &[(0., 0x1c2546, 1.), (0.52, 0x4a3b76, 1.), (1., 0xc27ba0, 1.)],
            )?;
            let highlight = linear(
                &target,
                pt(CX - 12., CY - 12.),
                pt(CX + 12., CY + 12.),
                &[
                    (0., 0xffffff, 0.),
                    (0.32, 0xffffff, 0.),
                    (0.46, 0xffffff, 0.18),
                    (0.6, 0xffffff, 0.),
                    (1., 0xffffff, 0.),
                ],
            )?;
            let shadow = target.CreateRadialGradientBrush(
                &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                    center: pt(CX, CY + 10.),
                    gradientOriginOffset: pt(0., 0.),
                    radiusX: 35.,
                    radiusY: 35.,
                },
                None,
                &stops(&target, &[(0., 0, 0.35), (0.55, 0, 0.30), (1., 0, 0.)])?,
            )?;
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 1.,
                    ..Default::default()
                },
                None,
            )?;
            let drop_icon = make_path(&factory, |s| {
                let p = |x, y| pt(CX - 9. + x, CY - 9. + y);
                s.BeginFigure(p(9., 2.2), D2D1_FIGURE_BEGIN_HOLLOW);
                s.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: p(9., 2.2),
                    point2: p(4., 8.),
                    point3: p(4., 11.2),
                });
                s.AddArc(&arc_end(CX + 5., CY + 2.2, 5., 5., false, false));
                s.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: p(14., 8.),
                    point2: p(9., 2.2),
                    point3: p(9., 2.2),
                });
                s.EndFigure(D2D1_FIGURE_END_CLOSED);
            })?;
            let power = make_path(&factory, |s| {
                s.BeginFigure(pt(7., 1.6), D2D1_FIGURE_BEGIN_HOLLOW);
                s.AddLine(pt(7., 6.6));
                s.EndFigure(D2D1_FIGURE_END_OPEN);
                s.BeginFigure(pt(4.1, 3.4), D2D1_FIGURE_BEGIN_HOLLOW);
                s.AddArc(&arc_end(9.9, 3.4, 4.6, 4.6, true, false));
                s.EndFigure(D2D1_FIGURE_END_OPEN);
            })?;
            Ok(Self {
                factory,
                target,
                surface: None,
                ink,
                accent,
                fallback,
                highlight,
                shadow,
                round,
                drop_icon,
                power,
                art: None,
                progress: None,
                previous_record_only: false,
                bounds: RECT::default(),
                text: Text::new()?,
            })
        }
    }
    fn solid(&self, rgb: u32, alpha: f32) -> &ID2D1SolidColorBrush {
        unsafe { self.ink.SetColor(&color(rgb, alpha)) }
        &self.ink
    }
    fn prepare(&mut self, dpi: u32, f: &Frame) -> Result<()> {
        unsafe {
            if self
                .surface
                .as_ref()
                .is_none_or(|(previous, _)| *previous != dpi)
            {
                let s = Surface::new(dpi)?;
                self.target.SetDpi(dpi as f32, dpi as f32);
                self.surface = Some((dpi, s));
                self.bounds = RECT::default();
                self.previous_record_only = false;
            }
            let surface = &self.surface.as_ref().unwrap().1;
            let compact = self.previous_record_only && f.record_only();
            let (right, bottom) = if compact {
                let pixels = |dip: f64| (dip * f64::from(dpi) / 96.).ceil() as i32;
                (pixels(CX as f64 + 36.), pixels(CY as f64 + 46.))
            } else {
                (surface.size.cx, surface.size.cy)
            };
            let bounds = RECT {
                left: 0,
                top: 0,
                right,
                bottom,
            };
            if self.bounds != bounds {
                self.target.BindDC(surface.dc, &bounds)?;
                self.bounds = bounds;
            }
            let step = (f.progress.unwrap_or(1.).clamp(0., 1.) * 4096.).round() as u16;
            if !f.has_media || step == 0 || step == 4096 {
                self.progress = None;
            } else if self.progress.as_ref().is_none_or(|(key, _)| *key != step) {
                let angle = TAU * f32::from(step) / 4096.;
                let path = make_path(&self.factory, |s| {
                    s.BeginFigure(pt(CX, CY - 20.5), D2D1_FIGURE_BEGIN_HOLLOW);
                    s.AddArc(&arc_end(
                        CX + 20.5 * angle.sin(),
                        CY - 20.5 * angle.cos(),
                        20.5,
                        20.5,
                        step > 2048,
                        true,
                    ));
                    s.EndFigure(D2D1_FIGURE_END_OPEN);
                })?;
                self.progress = Some((step, path));
            }
            match f.cover {
                None => self.art = None,
                Some(c) if self.art.as_ref().is_none_or(|(last, _)| last != &c.pixels) => {
                    if c.size > 0
                        && c.size <= 1024
                        && c.pixels.len() == (c.size * c.size * 4) as usize
                    {
                        let bdpi = 96. * c.size as f32 / 34.;
                        let image = self.target.CreateBitmap(
                            D2D_SIZE_U {
                                width: c.size,
                                height: c.size,
                            },
                            Some(c.pixels.as_ptr().cast()),
                            c.size * 4,
                            &D2D1_BITMAP_PROPERTIES {
                                pixelFormat: pixel_format(),
                                dpiX: bdpi,
                                dpiY: bdpi,
                            },
                        )?;
                        let brush = self.target.CreateBitmapBrush(&image, None, None)?;
                        self.art = Some((c.pixels.clone(), brush));
                    } else {
                        self.art = None
                    }
                }
                _ => {}
            }
        }
        if f.content > 0. {
            self.text.update(f.status)?
        }
        Ok(())
    }
    pub fn draw(&mut self, hwnd: HWND, origin: (i32, i32), dpi: u32, f: &Frame) -> Result<()> {
        self.prepare(dpi.max(1), f)?;
        let s = &self.surface.as_ref().unwrap().1;
        unsafe {
            self.target.BeginDraw();
            self.target.Clear(Some(&color(0, 0.)));
            self.paint(f);
            self.target.EndDraw(None, None)?;
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let dst = POINT {
                x: origin.0,
                y: origin.1,
            };
            let src = POINT::default();
            let info = UPDATELAYEREDWINDOWINFO {
                cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                pptDst: &dst,
                psize: &s.size,
                hdcSrc: s.dc,
                pptSrc: &src,
                crKey: COLORREF(0),
                pblend: &blend,
                dwFlags: ULW_ALPHA,
                prcDirty: &self.bounds,
                ..Default::default()
            };
            UpdateLayeredWindowIndirect(hwnd, &info).ok()?;
            self.previous_record_only = f.record_only();
            Ok(())
        }
    }
    fn paint(&self, f: &Frame) {
        let t = &self.target;
        let a = f.opacity.clamp(0., 1.);
        let base = transform(
            f.scale.0,
            f.scale.1,
            CX * (1. - f.scale.0) + f.dx,
            CY * (1. - f.scale.1) + f.dy,
        );
        let identity = transform(1., 1., 0., 0.);
        unsafe {
            t.SetTransform(&identity);
            if (0.0..=1.).contains(&f.ripple) {
                let radius = 22. * (1. + 1.1 * f.ripple);
                t.DrawEllipse(
                    &ellipse(CX, CY, radius, radius),
                    self.solid(0xc58bea, 0.42 * (1. - f.ripple)),
                    1.5,
                    None,
                );
            }
            if (0.0..=1.).contains(&f.splash) {
                let p = f.splash;
                let alpha = if p < 0.22 { p / 0.22 } else { (1. - p) / 0.78 };
                t.DrawEllipse(
                    &ellipse(CX, CY + 44., 20. * (0.2 + 1.5 * p), 7. * (0.2 + 0.35 * p)),
                    self.solid(0xc58bea, 0.7 * alpha),
                    1.,
                    None,
                );
            }
            if a <= 0. {
                return;
            }
            if a < 1. {
                t.PushLayer(
                    &D2D1_LAYER_PARAMETERS {
                        contentBounds: rect(0., 0., CANVAS.0 as f32, CANVAS.1 as f32),
                        maskTransform: identity,
                        opacity: a,
                        ..Default::default()
                    },
                    None,
                );
            }
            self.paint_body(f, base);
            if a < 1. {
                t.PopLayer();
            }
            t.SetTransform(&identity);
        }
    }

    fn paint_body(&self, f: &Frame, base: Matrix3x2) {
        let t = &self.target;
        unsafe {
            t.SetTransform(&base);
            t.FillEllipse(&ellipse(CX, CY + 10., 35., 35.), &self.shadow);
            let right = LEFT + f.width;
            t.FillRoundedRectangle(
                &pill(rect(LEFT, TOP, right, TOP + 44.), 22.),
                self.solid(0x141218, 0.88),
            );
            let edge = if f.hover {
                (0xc58bea, 0.55)
            } else {
                (0xffffff, 0.08)
            };
            t.DrawRoundedRectangle(
                &pill(rect(LEFT + 0.5, TOP + 0.5, right - 0.5, TOP + 43.5), 21.5),
                self.solid(edge.0, edge.1),
                1.,
                None,
            );
            self.paint_record(f);
            self.paint_controls(f, right, &base);
            self.paint_tooltip(f.tooltip);
        }
    }

    fn paint_record(&self, f: &Frame) {
        let t = &self.target;
        unsafe {
            let ring = ellipse(CX, CY, 20.5, 20.5);
            t.DrawEllipse(&ring, self.solid(0xffffff, 0.10), 2., None);
            if f.has_media {
                let fraction = f.progress.unwrap_or(1.).clamp(0., 1.);
                let progress: &ID2D1Brush = if f.playing && f.progress.is_some() {
                    &self.accent
                } else {
                    self.solid(0xffffff, 0.28)
                };
                if fraction >= 1. - 0.5 / 4096. {
                    t.DrawEllipse(&ring, progress, 2., None);
                } else if let Some((_, path)) = &self.progress {
                    t.DrawGeometry(path, progress, 2., &self.round);
                }
                let disc = ellipse(CX, CY, 17., 17.);
                let rotation = disc_rotation(f.angle);
                let brush: &ID2D1Brush = if let Some((_, b)) = &self.art {
                    b
                } else {
                    &self.fallback
                };
                brush.SetTransform(&rotation);
                t.FillEllipse(&disc, brush);
                for radius in [3., 6., 9., 12., 15.] {
                    t.DrawEllipse(
                        &ellipse(CX, CY, radius, radius),
                        self.solid(0, 0.12),
                        1.,
                        None,
                    );
                }
                t.FillEllipse(&disc, &self.highlight);
                t.FillEllipse(&ellipse(CX, CY, 3., 3.), self.solid(0x141218, 1.));
                t.DrawEllipse(
                    &ellipse(CX, CY, 3., 3.),
                    self.solid(0xffffff, 0.35),
                    1.5,
                    None,
                );
                if !f.playing {
                    t.FillEllipse(&disc, self.solid(0, 0.38));
                }
            } else {
                t.DrawGeometry(
                    &self.drop_icon,
                    self.solid(0xc58bea, f.breath),
                    1.6,
                    &self.round,
                );
            }
        }
    }

    fn paint_controls(&self, f: &Frame, right: f32, base: &Matrix3x2) {
        let t = &self.target;
        unsafe {
            let content = f.content.clamp(0., 1.);
            if content > 0. && right > LEFT + 50. {
                let opts = D2D1_DRAW_TEXT_OPTIONS_NONE;
                t.PushAxisAlignedClip(
                    &rect(LEFT + 44., TOP, right - 3., TOP + 44.),
                    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                );
                let button_left = right - 76.;
                if button_left > LEFT + 52. {
                    t.PushAxisAlignedClip(
                        &rect(LEFT + 49., TOP, button_left - 1., TOP + 44.),
                        D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    );
                    t.DrawTextLayout(
                        pt(LEFT + 50., TOP + 5.),
                        &self.text.main,
                        self.solid(0xf4ecf6, content),
                        opts,
                    );
                    if let Some((_, layout, natural)) = &self.text.status {
                        let squeeze = (88.0 / natural.max(1.)).min(1.);
                        let x = LEFT + 50.;
                        t.SetTransform(&transform(
                            base.M11 * squeeze,
                            base.M22,
                            base.M31 + x * (1. - squeeze) * base.M11,
                            base.M32,
                        ));
                        t.DrawTextLayout(
                            pt(x, TOP + 23.),
                            layout,
                            self.solid(0xa69db2, content),
                            opts,
                        );
                        t.SetTransform(base);
                    }
                    t.PopAxisAlignedClip();
                }
                t.FillRoundedRectangle(
                    &pill(rect(button_left, TOP + 5., right - 5., TOP + 39.), 17.),
                    self.solid(0xff6b81, content * if f.close_hover { 0.30 } else { 0.16 }),
                );
                let glyph = self.solid(0xffb1c0, content);
                t.SetTransform(&transform(
                    base.M11,
                    base.M22,
                    base.M31 + (button_left + 11.) * base.M11,
                    base.M32 + (TOP + 15.) * base.M22,
                ));
                t.DrawGeometry(&self.power, glyph, 1.6, &self.round);
                t.SetTransform(base);
                t.DrawTextLayout(
                    pt(button_left + 31., TOP + 5.),
                    &self.text.button,
                    glyph,
                    opts,
                );
                t.PopAxisAlignedClip();
            }
        }
    }

    fn paint_tooltip(&self, opacity: f32) {
        let t = &self.target;
        unsafe {
            if opacity > 0. {
                let k = opacity.clamp(0., 1.);
                let y = TOP + 44. + 10. - (1. - k) * 4.;
                let shape = pill(rect(CX - 60., y, CX + 60., y + 28.), 14.);
                t.FillRoundedRectangle(&shape, self.solid(0x141218, 0.90 * k));
                t.DrawRoundedRectangle(&shape, self.solid(0xffffff, 0.08 * k), 1., None);
                t.DrawTextLayout(
                    pt(CX - 60., y),
                    &self.text.hint,
                    self.solid(0xdcd3e4, k),
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_a_square_cover_keeps_its_center_on_the_spindle() {
        for angle in [0., 90., 175.5, 270., 360.] {
            let matrix = disc_rotation(angle);
            let x = 17. * matrix.M11 + 17. * matrix.M21 + matrix.M31;
            let y = 17. * matrix.M12 + 17. * matrix.M22 + matrix.M32;
            assert!((x - CX).abs() < 0.001 && (y - CY).abs() < 0.001);
        }
    }
}
