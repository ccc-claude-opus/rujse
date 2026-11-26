//! The built-in objects of the realm.

pub mod array;
pub mod array_methods;
pub mod bigint;
pub mod bigint_impl;
pub mod buffer;
pub mod collections;
pub mod date;
pub mod error;
pub mod function;
pub mod generator;
pub mod iterators;
pub mod json;
pub mod math;
pub mod number;
pub mod object;
pub mod promise;
pub mod proxy;
pub mod regexp;
pub mod string;

use crate::vm::Vm;

pub fn init(vm: &mut Vm) {
    function::init(vm);
    object::init(vm);
    error::init(vm);
    error::init_symbol(vm);
    error::init_boolean(vm);
    iterators::init(vm);
    array::init(vm);
    string::init(vm);
    number::init(vm);
    math::init(vm);
    json::init(vm);
    generator::init(vm);
    promise::init(vm);
    collections::init(vm);
    regexp::init(vm);
    date::init(vm);
    bigint::init(vm);
    buffer::init(vm);
    proxy::init(vm);
    object::init_reflect(vm);
    error::init_global(vm);
}
