#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SurfaceRole {
    Buffer,
    PersistentPanel,
    Transient,
}
