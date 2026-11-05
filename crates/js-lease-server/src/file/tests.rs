//! Unit tests of the file reader's pool checks.

use js_types::Dims;

use super::*;

fn section() -> Section {
    Section::open(toml::Table::new(), "pool \"x\": ".to_owned())
}

fn floor() -> Shape {
    let floor = Resources::new(1000, 1 << 30, 0, None).unwrap();
    Shape::new(floor, BTreeMap::new())
}

/// Asserts that a class that books part of a host is refused naming the pool and
/// `class`, with the reason the pool file of the flags gives, and that a class that
/// books whole hosts is served. No class a file can name books part of a host today,
/// so the check is tried on a class built here. It catches a mixed pool that is served
/// as if it booked whole hosts.
#[test]
fn a_class_that_books_part_of_a_host_is_refused() {
    let mixed = ResourceClass::new(
        ClassId::new("mixed").unwrap(),
        Admission::Book(Dims::new()),
        0,
    );
    let refused = section().class_served(&mixed).err().unwrap();
    assert_eq!(
        refused.to_string(),
        "pool \"x\": class: a mixed pool (a class that books part of a host) is not served by this server yet"
    );
    assert!(section().class_served(&ResourceClass::whole_node()).is_ok());
    assert!(section().class_served(&ResourceClass::gpu_host()).is_ok());
}

/// Asserts how the registry's pool is built: a whole-node pool of no machines is
/// refused naming the pool and `class` with the registry's own reason; a whole-node
/// pool of machines gets the dedicated settings; a pool of another class gets none; and
/// the host contract is set when asked for and absent when not. It catches a dedicated
/// pool whose settings are dropped when the registry refuses them.
#[test]
fn the_registry_pool_takes_the_class_and_the_contract() {
    let id = ProviderId::new("x").unwrap();
    let full = MemoryPercent::FULL;
    let whole = ResourceClass::whole_node();
    let refused = registry_pool(&section(), id.clone(), &whole, floor(), 0, None, full);
    let said = refused.err().unwrap().to_string();
    assert!(said.starts_with("pool \"x\": class: "), "{said}");
    assert!(said.ends_with("must be at least 1"), "{said}");
    let exec = Some(ExecMode::Container);
    let built = registry_pool(&section(), id.clone(), &whole, floor(), 2, exec, full).unwrap();
    assert_eq!(built.whole_node().unwrap().max_whole_node, 2);
    assert_eq!(built.exec(), exec);
    let gpu = ResourceClass::gpu_host();
    let plain = registry_pool(&section(), id, &gpu, floor(), 2, None, full).unwrap();
    assert!(plain.whole_node().is_none());
    assert_eq!(plain.exec(), None);
}
