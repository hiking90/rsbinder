use rsbinder::Parcelable;

#[derive(Parcelable)]
pub struct BareFd {
    pub fd: rsbinder::ParcelFileDescriptor,
}

#[derive(Parcelable)]
pub struct BareBinder {
    pub b: rsbinder::SIBinder,
}

fn main() {}
