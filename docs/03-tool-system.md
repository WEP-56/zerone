# 03 · 工具系统

对应源码:`src/tools/mod.rs`(契约)、`src/tools/*.rs`(五个工具)、
`src/workspace.rs`(文件访问层)、`src/util.rs`(清洗与截断)。

## Tool trait:四个方法,各有讲究

```rust
pub trait Tool: Send {
    fn name(&self) -> &'static str;      // 模型用它点名
    fn description(&self) -> String;     // ★ 工具的"提示工程"
    fn schema(&self) -> Value;           // 参数 JSON Schema
    fn execute(&self, args, ws, cancel) -> Result<String, String>;
}
```

**`description` 是给模型看的,不是给人看的**——它的质量直接决定模型
用不用、怎么用这个工具。写法上有三条经验(五个内置工具都是范例):

1. 说清"什么时候用/不用":`write_file` 明确"局部修改请用 edit_file";
2. 把不变量写进去:`edit_file` 强调"old_string 必须逐字符一致且唯一";
3. 动态信息动态生成:`run_command` 的 description 会随探测到的 shell
   变化(Git Bash 提示 POSIX 语法,PowerShell 提示没有 `&&`)。

**`execute` 的 `Err` 不是异常,是另一种 Observation**。
模型看到"old_string 出现了 3 次,请扩大上下文"之后会自己修正参数重试
——这是 agent 自愈能力的来源,所以错误文案必须"模型看得懂、能行动",
而不是堆栈。`ToolRegistry::execute` 连"工具名不存在"都走这条路
(回一句"未知工具,可用工具有:…"),Loop 永不因坏调用而崩溃。

## ToolRegistry:声明导出 + 按名分发

- `specs()` 把工具集导出成与厂商无关的 `ToolSpec{name, description, schema}`,
  三个 provider 适配器各自翻译成自家的声明格式(见 04);
- `execute()` 分发,并统一做两件收尾:`util::sanitize`(剥 ANSI 码/控制字符)
  与 `util::truncate_middle`(保头保尾截断,默认 24k 字符)——
  **任何工具都不用自己操心输出安全**。

注册新工具只需两行(完整教程见 06):

```rust
// tools/mod.rs
mod my_tool;                                  // 1. 声明模块
// default_registry() 里:
Box::new(my_tool::MyTool),                    // 2. 加进列表
```

## Workspace:为什么工具不许直接碰 std::fs

`Workspace` 现在只做三件事:持有 root、`resolve()` 相对路径、
提供 `read_text/write_text/read_dir_sorted` 等原语(带大小上限与
UTF-8 校验,错误文案面向模型)。看起来薄,但它是一批未来能力的
唯一挂载点:

- **沙箱/Guardrails**:在 `resolve()` 里拒绝逃出 root 的路径——一处改动,
  五个工具同时生效;
- **审计**:在读写原语处记录 agent 动过的每个文件;
- **Workspace Map**:扫描结构生成项目地图(配合 05 的 ContextProvider);
- **可测试性**:`tests/wire.rs` 直接 new 一个指向临时目录的 Workspace。

如果工具各自 `std::fs`,以上每件事都要改五处。这就是"所有 Tool 必须
通过 Workspace"的全部理由。

## 五个内置工具的设计要点

| 工具 | 要点 | 为什么 |
|---|---|---|
| `read_file` | 带行号(`  12 \| …`)、offset/limit 分页、超长行折断 | 行号帮模型给 edit_file 定位;分页教模型处理大文件而不是一口吞 |
| `list_dir` | 目录在前、depth≤4、跳过 `.git/target/node_modules`、500 条上限 | 防止一次列出几万条撑爆上下文 |
| `write_file` | 自动建父目录、汇报"新建/覆盖 + 字节 + 行数" | 汇报语句本身就是模型的确认信号 |
| `edit_file` | 唯一匹配否则报次数、`replace_all` 兜底、**LF 域匹配 + 还原 CRLF** | 见下 |
| `run_command` | shell 探测、临时 .ps1、超时/取消杀进程树、双线程读管道、UTF-8 前导码 | 见下 |

### edit_file 与 CRLF(Windows 的头号坑)

`read_file` 展示给模型的内容不带 `\r`,模型照着拼 `old_string`;
而磁盘文件很可能是 CRLF。若拿原始字节匹配,结果就是大面积的
"明明看见了却找不到"。解法(`edit_file.rs`):**统一转到 LF 域做
匹配替换,写回时若原文件以 CRLF 为主则整体还原**。
配套测试 `crlf_file_matches_lf_pattern_and_stays_crlf` 锁死这个行为。

"唯一匹配"规则同样重要:出现 N 次直接报错并要求扩大上下文,
宁可多一轮往返,不做"改了第一处"这种静默错误。

### run_command 的五道保险

1. **shell 探测**(`detect_shell`):在标准安装路径找 Git Bash
   (刻意不搜 PATH——`System32\bash.exe` 是 WSL,语义完全不同),
   找不到退回 PowerShell;可用 `shell =` 配置强制。
2. **PowerShell 走临时 .ps1 文件**(带 UTF-8 BOM):`-Command` 内联要穿
   Rust→CreateProcess→PowerShell 三层引号规则,多行必炸;脚本文件绕开
   一切引号问题。前导码强制 UTF-8 输出,否则中文 Windows 全是 GBK 乱码。
3. **stdin 接 null**:交互式命令立刻拿到 EOF,而不是挂死整个 agent。
4. **双线程读管道**:stdout/stderr 各一个读线程;单线程顺序读,
   另一管道写满 64KB 缓冲后子进程会永久阻塞(经典死锁)。
5. **超时(默认 120s)与取消都 `taskkill /T /F` 杀整棵进程树**:
   只杀父进程会留孤儿占端口。非零退出码走 `Err`(模型据此自愈)。

已知取舍:每次调用独立进程,`cd` 不保留(提供 `cwd` 参数替代)。
想要持久 shell?那是一个很好的进阶练习:开一个常驻 bash 进程,
用哨兵字符串切分每条命令的输出。

## 扩展指引

- **加只读工具(grep/glob)**:半小时工作量,照 06 做。
- **权限/审批系统**:唯一正确挂载点是 `ToolRegistry::execute` 的开头——
  在真正分发前,根据 (工具名, 参数) 决定放行/询问/拒绝。询问需要一条
  "Runtime→前端→Runtime"的往返:加一对事件/命令(如 `ApprovalRequest` /
  `ApprovalReply`),Runtime 阻塞等待回复即可,Loop 本体不用改。
- **并行工具执行**:目前串行(可读性优先)。模型常一次发多个只读调用,
  把 for 循环换成 scoped threads 即可,但要保证结果**按原顺序**写回历史。
- **工具级超时/预算**:在 registry 层包一圈计时即可,工具无感知。
