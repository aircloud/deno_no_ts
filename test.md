## 主要删除点：

* 去掉 opEmit 相关内容
* 去掉 lsp
* 去掉 tsc
* 去掉 deno run 以外的其他命令，比如 compile 和 bundle
* 去掉 standalone


* 测试代码：

./deno --log-level debug run main.ts

* 测试代码并使用浏览器调试：

./deno run --inspect-brk --allow-read  main.js

* 直接运行：

./deno run main.ts
