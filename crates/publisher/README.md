# publisher

This crate imports the library portion of
[`margic/director-narrative-core`](https://github.com/margic/director-narrative-core),
from branch `devin/1789000598-local-destination`, including the publisher
pipeline from PR [#81](https://github.com/margic/director-narrative-core/pull/81).

Imported source SHA:

```text
f88ee0d7bb100fdbd141d19b00049b12ad4dac07
```

The publisher, publisher UI, and replay binaries were intentionally omitted.
Tray/UI code and its `eframe`/`egui` dependencies are not part of this
library-only crate. The imported code was reformatted with `cargo fmt` for this
workspace's CI.
