use rsbinder::Parcelable;

#[derive(Parcelable, Default)]
pub struct Bad {
    pub x: Option<i32>,
}

fn main() {}
