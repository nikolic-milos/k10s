use crate::Rect;

#[test]
fn touching_edges_are_contained_without_overlapping_the_neighbor() {
    let viewport = Rect::new(0.0, 0.0, 10.0, 10.0);
    for neighbor in [
        Rect::new(-10.0, 0.0, 10.0, 10.0),
        Rect::new(10.0, 0.0, 10.0, 10.0),
        Rect::new(0.0, -10.0, 10.0, 10.0),
        Rect::new(0.0, 10.0, 10.0, 10.0),
    ] {
        assert!(!viewport.intersects(&neighbor));
        assert!(!neighbor.intersects(&viewport));
        assert!(!viewport.contains(&neighbor));
    }
    assert!(viewport.contains(&viewport));
    assert!(viewport.intersects(&viewport));
    assert!(viewport.contains(&Rect::new(10.0, 10.0, 0.0, 0.0)));
    assert!(!viewport.intersects(&Rect::new(10.0, 10.0, 0.0, 0.0)));
    let crossing = Rect::new(9.0, 9.0, 2.0, 2.0);
    assert!(viewport.intersects(&crossing));
    assert!(crossing.intersects(&viewport));
    assert!(!viewport.contains(&crossing));
}

#[test]
fn an_unordered_coordinate_neither_overlaps_nor_contains() {
    let viewport = Rect::new(0.0, 0.0, 10.0, 10.0);
    for invalid in [
        Rect::new(f32::NAN, 1.0, 2.0, 2.0),
        Rect::new(1.0, f32::NAN, 2.0, 2.0),
        Rect::new(1.0, 1.0, f32::NAN, 2.0),
        Rect::new(1.0, 1.0, 2.0, f32::NAN),
    ] {
        assert!(!viewport.intersects(&invalid));
        assert!(!invalid.intersects(&viewport));
        assert!(!viewport.contains(&invalid));
        assert!(!invalid.contains(&viewport));
    }
}
