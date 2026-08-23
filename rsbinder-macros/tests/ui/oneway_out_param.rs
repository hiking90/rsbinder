use rsbinder::interface;

#[interface]
pub trait IBad {
    #[oneway]
    fn fill(&self, values: &mut Vec<i32>) -> rsbinder::BinderResult<()>;
}

fn main() {}
