# Tier 开发约定

- 代码位于 D 盘，构建、测试和 Git 操作在 Ubuntu 中执行。将 Cargo 构建目录放在 Linux 文件系统。
- 优先复用现有实现与维护良好的组件。变更围绕当前需求；同步删除失效的实现和文档。
- 文档记录使用方法、数据保证和影响运维的边界。提交面向最终行为，不保留试验记录和过渡兼容层。
- 合入前执行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `cargo test`。涉及文件系统语义的变更还需通过 SMB 入口验证。
