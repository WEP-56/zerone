# CacheBoard 测试项目需求书

## 1. 项目概述

CacheBoard 是一个本地运行的轻量工单管理 CLI，用于管理个人或小团队的软件开发任务。

项目规模控制在中小型：

- 15–25 个 Python 源码与测试文件；
- 约 1500–2500 行有效代码；
- 不依赖网络服务或第三方 Python 包；
- 使用 JSON 文件持久化；
- 使用 `unittest` 提供完整自动化测试。

项目重点不是界面，而是清晰的领域模型、分层结构、可靠的文件写入、可测试的命令行为，以及适量的跨模块修改任务。

## 2. 技术约束

- Python 3.11 或更高版本；
- 仅使用 Python 标准库；
- 使用 `pyproject.toml` 描述项目；
- 源码采用 `src` 布局；
- 测试命令固定为：

```powershell
python -m unittest discover -s tests -v
```

- CLI 入口固定为：

```powershell
python -m cacheboard --help
```

- 必须同时兼容 Windows、Linux 和 macOS；
- 所有文件使用 UTF-8；
- 不使用全局可变状态；
- 领域逻辑不得直接读取 `sys.argv`、环境变量或真实系统时间。

## 3. 目录结构

推荐结构如下，允许实现时做小幅调整，但必须保持领域、存储、服务和 CLI 的边界：

```text
cacheboard/
  pyproject.toml
  README.md
  CHANGELOG.md
  src/
    cacheboard/
      __init__.py
      __main__.py
      cli.py
      errors.py
      clock.py
      domain/
        __init__.py
        issue.py
        enums.py
        query.py
      services/
        __init__.py
        board.py
        reports.py
      storage/
        __init__.py
        json_repository.py
        migrations.py
      presentation/
        __init__.py
        table.py
        json_output.py
  tests/
    test_issue.py
    test_query.py
    test_repository.py
    test_board_service.py
    test_reports.py
    test_cli.py
  fixtures/
    sample-board.json
```

## 4. 领域模型

### 4.1 Issue

每个工单包含：

| 字段 | 类型 | 规则 |
|---|---|---|
| `id` | string | 格式为 `CB-0001`，在同一数据文件内递增且不复用 |
| `title` | string | 去除首尾空白后长度为 1–120 |
| `description` | string | 可为空，最大 4000 字符 |
| `status` | enum | `todo`、`doing`、`done`、`archived` |
| `priority` | enum | `low`、`medium`、`high`、`urgent` |
| `tags` | list[string] | 小写、去重、排序；每项匹配 `[a-z0-9][a-z0-9-]{0,31}` |
| `assignee` | string/null | 可为空；非空时去除首尾空白 |
| `created_at` | string | UTC ISO-8601，使用 `Z` 后缀 |
| `updated_at` | string | UTC ISO-8601，不能早于 `created_at` |

### 4.2 状态转换

允许的转换：

```text
todo -> doing | archived
doing -> todo | done | archived
done -> doing | archived
archived -> todo
```

非法转换必须返回明确的领域错误，不能静默修改。

### 4.3 时间

定义 `Clock` 接口和 `SystemClock` 实现。测试使用 `FixedClock`，不得通过 patch 全局时间函数完成测试。

## 5. JSON 存储格式

根对象格式：

```json
{
  "schema_version": 1,
  "next_issue_number": 3,
  "issues": []
}
```

要求：

- 文件不存在时，只有 `init` 可以显式创建；
- 写入采用“同目录临时文件 + flush + 原子替换”；
- 写入后的 JSON 使用 2 空格缩进，并以换行结束；
- Issue 在文件中按数字 ID 升序保存；
- 未知 `schema_version` 必须拒绝打开并提示支持的版本；
- JSON 损坏时不得覆盖原文件；
- Repository 负责序列化，不包含业务状态转换逻辑；
- 预留 `migrations.py`，但 v1 暂不需要实际迁移。

## 6. CLI 命令

所有命令支持全局参数：

```text
--db PATH          数据文件，默认 .cacheboard/board.json
--format FORMAT    table | json，默认 table
```

### 6.1 初始化

```powershell
python -m cacheboard init
```

- 创建空数据文件；
- 文件已存在时拒绝覆盖；
- 成功输出数据文件路径。

### 6.2 新增工单

```powershell
python -m cacheboard add "修复登录超时" `
  --description "刷新 token 后重试一次" `
  --priority high `
  --tag auth --tag backend `
  --assignee alice
```

- 默认状态为 `todo`；
- 默认优先级为 `medium`；
- 成功时输出完整工单。

### 6.3 查看工单

```powershell
python -m cacheboard show CB-0001
```

不存在时返回退出码 3。

### 6.4 列表与筛选

```powershell
python -m cacheboard list
python -m cacheboard list --status doing --priority high
python -m cacheboard list --tag backend --assignee alice
python -m cacheboard list --text token --sort priority
```

筛选规则：

- 不同类型条件之间是 AND；
- 多个 `--tag` 之间也是 AND；
- `--text` 对标题和描述进行不区分大小写的包含匹配；
- 默认不显示 `archived`，使用 `--include-archived` 后显示；
- 默认排序为 status、priority、数字 ID；
- `--sort` 支持 `id`、`priority`、`updated`。

优先级排序固定为：

```text
urgent > high > medium > low
```

### 6.5 更新字段

```powershell
python -m cacheboard update CB-0001 --title "新的标题"
python -m cacheboard update CB-0001 --add-tag api --remove-tag backend
python -m cacheboard update CB-0001 --assignee bob
python -m cacheboard update CB-0001 --clear-assignee
```

- 至少提供一个修改项；
- `--assignee` 与 `--clear-assignee` 互斥；
- 修改失败时不得产生部分写入；
- 有效修改必须更新 `updated_at`；
- 修改后的值与原值完全相同时，不更新 `updated_at`。

### 6.6 移动状态

```powershell
python -m cacheboard move CB-0001 doing
python -m cacheboard move CB-0001 done
```

必须遵守领域状态转换规则。

### 6.7 报表

```powershell
python -m cacheboard report summary
python -m cacheboard report assignees
```

`summary` 输出：

- 各状态数量；
- 各优先级数量；
- 未分配工单数量；
- 未完成且为 high/urgent 的工单数量。

`assignees` 按负责人输出 todo、doing、done 数量，未分配使用 `(unassigned)`。

## 7. 输出与退出码

### 7.1 Table 输出

- 列宽根据当前结果计算；
- 标题超过 40 个显示宽度时截断并附加 `...`；
- 空结果输出 `No issues found.`；
- 不依赖终端颜色保证信息完整。

### 7.2 JSON 输出

- stdout 只输出合法 JSON；
- 错误写入 stderr；
- 单个工单输出对象，列表输出数组，报表输出对象；
- 字段名与存储模型一致。

### 7.3 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 成功 |
| 2 | 命令参数错误 |
| 3 | 工单不存在 |
| 4 | 领域规则冲突 |
| 5 | 存储或数据格式错误 |

## 8. 错误设计

至少定义以下异常：

- `CacheBoardError`；
- `ValidationError`；
- `IssueNotFoundError`；
- `InvalidTransitionError`；
- `StorageError`；
- `UnsupportedSchemaError`。

CLI 层负责把异常映射为退出码；领域层和存储层不得直接打印。

错误信息需要包含可执行的上下文，例如工单 ID、无效字段或数据文件路径，但不得输出 Python traceback，除非显式启用开发调试模式。

## 9. 测试要求

至少覆盖：

- Issue 字段校验和 tag 规范化；
- 每条合法与非法状态转换；
- ID 单调递增且归档后不复用；
- Repository 空文件、损坏 JSON、未知 schema；
- 原子替换失败时原文件保持完整；
- 组合筛选和三种排序；
- update 无变化时不更新时间；
- table 截断与空结果；
- JSON 输出可被 `json.loads` 解析；
- 每个领域异常对应正确退出码；
- CLI 的完整 `init -> add -> update -> move -> list -> report` 流程。

测试必须使用临时目录，不得读写用户真实的 `.cacheboard` 目录。

## 10. README 要求

项目 README 至少包含：

- 安装与运行要求；
- 5 分钟快速开始；
- 所有命令的常用示例；
- 数据文件格式说明；
- 退出码表；
- 如何运行测试；
- 架构边界简述；
- 当前限制。

## 11. 分阶段工作包

为了保持每次修改范围清晰，按以下顺序实施。

### 工作包 A：骨架与领域模型

- 建立目录、`pyproject.toml` 和入口；
- 实现 enum、Issue、Clock 和领域错误；
- 完成领域单元测试。

### 工作包 B：JSON Repository

- 实现 schema v1 读写、ID 分配和原子替换；
- 覆盖损坏数据与失败保护测试；
- 添加 `fixtures/sample-board.json`。

### 工作包 C：服务层与基础 CLI

- 实现 init、add、show、update、move；
- 完成异常到退出码的映射；
- 增加服务层和 CLI 测试。

### 工作包 D：查询与展示

- 实现组合筛选、排序、table 和 JSON 输出；
- 补充边界测试。

### 工作包 E：报表与文档

- 实现 summary 和 assignees 报表；
- 完成 README、CHANGELOG；
- 运行全量测试并清理临时文件。

## 12. 最终验收

项目完成时必须满足：

```powershell
python -m unittest discover -s tests -v
python -m cacheboard --help
```

并手工验证：

1. 在空目录初始化数据文件；
2. 新增至少 6 个不同状态、优先级、标签和负责人的工单；
3. 执行组合筛选并检查顺序；
4. 修改字段并完成合法状态流转；
5. 尝试一次非法状态流转，确认文件未变化；
6. 分别生成 table 和 JSON 报表；
7. 确认损坏 JSON 不会被程序覆盖。

验收不要求发布到 PyPI，不要求 Web UI，不要求数据库，不要求用户认证，也不要求并发多进程写入。
