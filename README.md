# Deno

**去掉了 TS 支持的 Deno，只保留运行和调试本地 JS 文件的能力**

该库的意义：

* 可以简单地基于此进行进一步裁剪，从而变成一个嵌入 PC 客户端应用的 JS 运行时

> 该项工作正在进行中，目前有一些正在完善的地方，例如测试部分目前未完善

---

[![Build Status - Cirrus][]][Build status] [![Twitter handle][]][Twitter badge]

<img align="right" src=docs/images/deno3.png height="150px">

Deno is a _simple_, _modern_ and _secure_ runtime for **JavaScript** and
**TypeScript** that uses V8 and is built in Rust.

### Features

- Secure by default. No file, network, or environment access, unless explicitly
  enabled.
- Supports TypeScript out of the box.
- Ships only a single executable file.
- Built-in utilities like a dependency inspector (deno info) and a code
  formatter (deno fmt).
- Set of reviewed standard modules that are guaranteed to work with
  [Deno](https://deno.land/std/).

### Install

Shell (Mac, Linux):

```sh
curl -fsSL https://deno.land/x/install/install.sh | sh
```

PowerShell (Windows):

```powershell
iwr https://deno.land/x/install/install.ps1 -useb | iex
```

[Homebrew](https://formulae.brew.sh/formula/deno) (Mac):

```sh
brew install deno
```

[Chocolatey](https://chocolatey.org/packages/deno) (Windows):

```powershell
choco install deno
```

Build and install from source using [Cargo](https://crates.io/crates/deno):

```sh
cargo install deno
```

See
[deno_install](https://github.com/denoland/deno_install/blob/master/README.md)
and [releases](https://github.com/denoland/deno/releases) for other options.

### Getting Started

Try running a simple program:

```sh
deno run https://deno.land/std/examples/welcome.ts
```

Or a more complex one:

```ts
import { serve } from "https://deno.land/std/http/server.ts";
const s = serve({ port: 8000 });
console.log("http://localhost:8000/");
for await (const req of s) {
  req.respond({ body: "Hello World\n" });
}
```

You can find a more in depth introduction, examples, and environment setup
guides in the [manual](https://deno.land/manual).

More in-depth info can be found in the runtime
[documentation](https://doc.deno.land).

### Contributing

We appreciate your help!

To contribute, please read our
[guidelines](https://github.com/denoland/deno/blob/master/docs/contributing/style_guide.md).

[Build Status - Cirrus]: https://github.com/denoland/deno/workflows/ci/badge.svg?branch=master&event=push
[Build status]: https://github.com/denoland/deno/actions
[Twitter badge]: https://twitter.com/intent/follow?screen_name=deno_land
[Twitter handle]: https://img.shields.io/twitter/follow/deno_land.svg?style=social&label=Follow
