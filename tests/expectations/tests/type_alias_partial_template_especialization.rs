/* automatically generated by rust-bindgen */


#![allow(non_snake_case)]


pub type MaybeWrapped<A> = A;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct Rooted<T> {
    pub ptr: MaybeWrapped<T>,
}
impl <T> Default for Rooted<T> {
    fn default() -> Self { unsafe { ::std::mem::zeroed() } }
}
