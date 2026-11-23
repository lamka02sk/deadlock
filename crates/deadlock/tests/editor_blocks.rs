//! Geometry tests for the editor block-widget patch in `vendor/gpui-base`. That
//! crate's own test suite needs GPUI's git test-support build, so its block map,
//! which depends only on GPUI types, is compiled and tested here.

extern crate gpui_kit as gpui;

#[allow(dead_code)]
#[path = "../../../vendor/gpui-base/src/input/editor/display_map/block_map.rs"]
mod block_map;
