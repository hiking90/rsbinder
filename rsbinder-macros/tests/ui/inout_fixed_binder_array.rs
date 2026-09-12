use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, #[inout] v: &mut [rsbinder::ParcelFileDescriptor; 3])
        -> rsbinder::BinderResult<()>;
}

fn main() {}
