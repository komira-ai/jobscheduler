//! The accelerator an agent states for its shape, in wire form.

mod common;

use common::hello;

/// Asserts that a shape with a GPU class states that accelerator with the vendor and
/// model the class names, the count of GPUs, and the least memory of the listed devices,
/// and that a shape with no class, or no devices, states what it can.
#[test]
fn wire_accelerator_states_class_count_and_least_device_memory() {
    let frame = hello();
    let stated = frame
        .shape
        .wire_accelerator(&frame.gpu_devices)
        .expect("the shape has a class");
    let derived = frame.accelerator().expect("the shape has a class");
    assert_eq!(stated.vendor, derived.vendor.to_string());
    assert_eq!(stated.model, derived.model.to_string());
    assert_eq!(stated.count, 1);
    assert_eq!(stated.memory_per_device_bytes, Some(1 << 36));

    let bare = frame
        .shape
        .wire_accelerator(&[])
        .expect("the class stands without devices");
    assert_eq!(bare.memory_per_device_bytes, None);

    let mut classless = frame.shape.clone();
    classless.gpu_class = None;
    assert_eq!(classless.wire_accelerator(&frame.gpu_devices), None);
}
