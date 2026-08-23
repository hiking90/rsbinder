use rsbinder::BinderEnum;

#[derive(BinderEnum)]
#[repr(i32)]
pub enum Mode {
    Fast,
}

fn main() {}
