use rsbinder::Parcelable;

#[derive(Parcelable)]
pub struct BareElements {
    pub tags: Option<Vec<String>>,
}

#[derive(Parcelable)]
pub struct WrappedElements {
    pub tags: Vec<Option<String>>,
}

#[derive(Parcelable)]
pub struct BareSlots {
    pub fds: [rsbinder::ParcelFileDescriptor; 3],
}

fn main() {}
