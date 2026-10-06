//! The audit face: the management event record -- one row per
//! admin/user write on the registry faces. Request-time accounting
//! (request audit rows, usage extraction, budget sums) lives
//! outside this face.

pub mod management_event;
