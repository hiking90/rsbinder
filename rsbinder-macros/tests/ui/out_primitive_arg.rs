use rsbinder::interface;

#[interface]
pub trait IBad {
    fn fill(&self, out: &mut i32) -> rsbinder::BinderResult<()>;
}

fn main() {}
