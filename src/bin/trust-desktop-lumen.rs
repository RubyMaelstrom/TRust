// Keep the experimental frontend byte-for-byte on the production desktop
// adapter. Its distinct Cargo target name is the only selector; all browser,
// presentation, input, and renderer behavior remains shared.
include!("trust-desktop.rs");
