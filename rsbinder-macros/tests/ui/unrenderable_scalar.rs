use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, n: u32) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IWide {
    fn f(&self, n: u128) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IBadReturn {
    fn f(&self) -> rsbinder::BinderResult<u64>;
}

#[interface]
pub trait IBadElement {
    fn f(&self, v: &[u32]) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IBareByte {
    fn f(&self, n: u8) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IByteArrayElement {
    fn f(&self, v: &[i8]) -> rsbinder::BinderResult<()>;
}

#[derive(rsbinder::Parcelable)]
pub struct Wide {
    pub n: u128,
}

fn main() {}
