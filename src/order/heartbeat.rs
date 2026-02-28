// Heartbeat management is handled automatically by the SDK when the
// `heartbeats` feature is enabled. The SDK sends heartbeats at the
// configured interval (default 5s) and cancels all orders if a
// heartbeat fails.
//
// This module provides monitoring on top of the SDK's built-in heartbeat.
