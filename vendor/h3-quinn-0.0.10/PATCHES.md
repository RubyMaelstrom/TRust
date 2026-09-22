# TRust transport fix

Based on crates.io h3-quinn 0.0.10 (hyperium/h3, MIT; LICENSE retained).

The upstream receive adapter moves its Quinn stream into an owned future.
While a read is pending, both `recv_id` and `stop_sending` unwrap `None`.
This breaks ordinary HTTP/3 request cancellation (RFC 9114 §4.1.1).

Keep the stream owned and poll a borrowed, stack-pinned `read_chunk` future.
Quinn explicitly documents this operation as cancel-safe. Reads remain
zero-copy and no longer need a boxed future allocation per receive stream.
There are no wire protocol, TLS, or certificate policy changes.

Regression coverage: TRust's `http3_cancel_pending_read_stops_both_halves`
test cancels during a pending read and verifies stream-local cancellation
while another request still succeeds on the same connection.

The unused upstream readme is omitted from the normalized Cargo manifest.
