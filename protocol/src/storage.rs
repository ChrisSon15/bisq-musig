use std::convert::Infallible;

/// A simple utility trait to allow structs to be polymorphic over the storage type of their fields,
/// to facilitate passing data to and from functions by reference or value, decided statically for
/// maximum efficiency. (This avoids needless cloning of fields, the obvious alternative being to
/// make each field a [`std::borrow::Cow`], but the latter is dynamic and wastes storage space.)
pub trait Storage {
    type Store<'a, T: 'a>;
}

/// Hold the struct fields by reference.
pub struct ByRef(Infallible);

/// Hold the struct fields by value.
pub struct ByVal(Infallible);

impl Storage for ByRef {
    // It isn't ideal to make the lifetime a type parameter here, instead of making it a parameter
    // of the [`ByRef`] storage type, as it interferes with the use of the [`ByVal`] storage type,
    // but it's not clear how to get the latter to compile.
    type Store<'a, T: 'a> = &'a T;
}

impl Storage for ByVal {
    type Store<'a, T: 'a> = T;
}
