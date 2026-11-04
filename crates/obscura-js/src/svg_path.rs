use kurbo::{BezPath, ParamCurve, ParamCurveArclen, Point, Shape, SvgArc, Vec2};
use svgtypes::{PathParser, PathSegment};

// Bound synchronous geometry work from untrusted path data. Browser path
// coordinates are floats; reject numbers that cannot safely enter that range.
const MAX_SEGMENTS: usize = 10_000;
const MAX_COORDINATE: f64 = 1.0e12;

fn point(current: Point, absolute: bool, x: f64, y: f64) -> Point {
    if absolute { Point::new(x, y) } else { current + Vec2::new(x, y) }
}

fn valid(point: Point) -> bool {
    point.x.is_finite() && point.y.is_finite()
        && point.x.abs() <= MAX_COORDINATE && point.y.abs() <= MAX_COORDINATE
}

/// Return total length and a point at the requested distance. `None` is an
/// empty path; a valid prefix is retained when a later command is malformed.
pub(crate) fn metrics(data: &str, distance: f64) -> Option<(f64, Point)> {
    let mut path = BezPath::new();
    let mut current = Point::ORIGIN;
    let mut start = Point::ORIGIN;
    let mut first = None;
    let mut cubic_control: Option<Point> = None;
    let mut quad_control: Option<Point> = None;
    for parsed in PathParser::from(data).take(MAX_SEGMENTS) {
        let Ok(segment) = parsed else { break; };
        match segment {
            PathSegment::MoveTo { abs, x, y } => {
                let p = point(current, abs, x, y);
                if !valid(p) { break; }
                path.move_to(p); current = p; start = p;
                if first.is_none() { first = Some(p); }
                cubic_control = None; quad_control = None;
            }
            PathSegment::LineTo { abs, x, y } => {
                let p = point(current, abs, x, y);
                if !valid(p) || first.is_none() { break; }
                path.line_to(p); current = p; cubic_control = None; quad_control = None;
            }
            PathSegment::HorizontalLineTo { abs, x } => {
                let p = Point::new(if abs { x } else { current.x + x }, current.y);
                if !valid(p) || first.is_none() { break; }
                path.line_to(p); current = p; cubic_control = None; quad_control = None;
            }
            PathSegment::VerticalLineTo { abs, y } => {
                let p = Point::new(current.x, if abs { y } else { current.y + y });
                if !valid(p) || first.is_none() { break; }
                path.line_to(p); current = p; cubic_control = None; quad_control = None;
            }
            PathSegment::CurveTo { abs, x1, y1, x2, y2, x, y } => {
                let a = point(current, abs, x1, y1); let b = point(current, abs, x2, y2);
                let p = point(current, abs, x, y);
                if !valid(a) || !valid(b) || !valid(p) || first.is_none() { break; }
                path.curve_to(a, b, p); current = p; cubic_control = Some(b); quad_control = None;
            }
            PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
                let a = cubic_control.map_or(current, |c| current + (current - c));
                let b = point(current, abs, x2, y2); let p = point(current, abs, x, y);
                if !valid(a) || !valid(b) || !valid(p) || first.is_none() { break; }
                path.curve_to(a, b, p); current = p; cubic_control = Some(b); quad_control = None;
            }
            PathSegment::Quadratic { abs, x1, y1, x, y } => {
                let a = point(current, abs, x1, y1); let p = point(current, abs, x, y);
                if !valid(a) || !valid(p) || first.is_none() { break; }
                path.quad_to(a, p); current = p; quad_control = Some(a); cubic_control = None;
            }
            PathSegment::SmoothQuadratic { abs, x, y } => {
                let a = quad_control.map_or(current, |c| current + (current - c));
                let p = point(current, abs, x, y);
                if !valid(a) || !valid(p) || first.is_none() { break; }
                path.quad_to(a, p); current = p; quad_control = Some(a); cubic_control = None;
            }
            PathSegment::EllipticalArc { abs, rx, ry, x_axis_rotation, large_arc, sweep, x, y } => {
                let p = point(current, abs, x, y);
                if !valid(p) || !rx.is_finite() || !ry.is_finite() || !x_axis_rotation.is_finite()
                    || rx.abs() > MAX_COORDINATE || ry.abs() > MAX_COORDINATE || first.is_none() { break; }
                let arc = SvgArc { from: current, to: p, radii: Vec2::new(rx.abs(), ry.abs()),
                    x_rotation: (x_axis_rotation % 360.0).to_radians(), large_arc, sweep };
                if current != p {
                    if rx.abs() < 1e-9 || ry.abs() < 1e-9 { path.line_to(p); }
                    else if let Some(arc) = kurbo::Arc::from_svg_arc(&arc) {
                        let tolerance = arc.radii.x.abs().max(arc.radii.y.abs()).max(1.0) * 1e-6;
                        arc.to_cubic_beziers(tolerance, |a, b, p| path.curve_to(a, b, p));
                    } else { path.line_to(p); }
                }
                current = p; cubic_control = None; quad_control = None;
            }
            PathSegment::ClosePath { .. } => {
                if first.is_none() { break; }
                path.close_path(); current = start; cubic_control = None; quad_control = None;
            }
        }
    }
    let first = first?;
    let segments: Vec<_> = path.segments().map(|segment| {
        let bounds = segment.bounding_box();
        let accuracy = (bounds.width().abs().max(bounds.height().abs()) * 1e-7).max(1e-12);
        let length = segment.arclen(accuracy);
        (segment, length, accuracy)
    }).collect();
    let total: f64 = segments.iter().map(|(_, length, _)| length).sum();
    if !total.is_finite() { return None; }
    if distance <= 0.0 || total == 0.0 { return Some((total, first)); }
    let mut remaining = distance.min(total);
    let mut last = first;
    for (segment, length, accuracy) in segments {
        if length <= 0.0 { continue; }
        if remaining <= length {
            return Some((total, segment.eval(segment.inv_arclen(remaining, accuracy))));
        }
        remaining -= length;
        last = segment.eval(1.0);
    }
    Some((total, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_close_and_smooth_curves_have_expected_geometry() {
        let (length, p) = metrics("M10 20 h80 v30 h-80 z", 120.0).unwrap();
        assert!((length - 220.0).abs() < 1e-6);
        assert!((p.x - 80.0).abs() < 1e-6 && (p.y - 50.0).abs() < 1e-6);
        let (_, p) = metrics("M1 2 Q20 50 40 0 S70 20 90 0", 1e9).unwrap();
        assert_eq!(p, Point::new(90.0, 0.0));
        assert!(metrics("", 0.0).is_none());
    }

    #[test]
    fn curves_arcs_and_mixed_smooth_commands_match_chromium_fixture() {
        for (data, chrome_length) in [
            ("M0 0 C10 100 80 -40 100 20 S160 90 190 0", 295.1578369140625),
            ("M0 0 Q40 100 80 0 T160 0", 268.11798095703125),
            ("M5 20 A45 20 37 1 1 100 70 a20 35 -25 0 0 40 20", 188.1867218017578),
            ("M1 2 Q20 50 40 0 S70 20 90 0", 119.69662475585938),
        ] {
            let (length, _) = metrics(data, 0.0).unwrap();
            assert!((length - chrome_length).abs() < 0.04, "{data}: {length} vs {chrome_length}");
        }
        let (_, point) = metrics("M5 20 A45 20 37 1 1 100 70 a20 35 -25 0 0 40 20", 188.1867218017578 / 2.0).unwrap();
        assert!((point.x - 81.27568817138672).hypot(point.y - 36.945762634277344) < 0.04);
    }
}
