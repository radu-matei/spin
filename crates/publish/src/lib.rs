//! Functions for publishing Spin applications.
#![deny(missing_docs)]

/// Publish a Spin application to Bindle.
pub mod bindle;

/// Publish a Spin application to an OCI registry.
pub mod oci;

fn test() {
    let x = 3;
}
