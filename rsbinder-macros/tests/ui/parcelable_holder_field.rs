use rsbinder::Parcelable;

#[derive(Parcelable, Default)]
pub struct Bad {
    pub ext: rsbinder::ParcelableHolder,
}

fn main() {}
