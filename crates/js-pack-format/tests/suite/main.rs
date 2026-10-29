//! Tests of the pack format, in one binary.
//!
//! `golden` pins the bytes against a reference assembler written from the layout
//! alone; `writer` and `verify` cover the writer and every refusal of the strict check;
//! `wire` covers the grant, the report and their encoding.

mod common;
mod golden;
mod verify;
mod wire;
mod writer;
