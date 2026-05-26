# docx2md_tui

一个用 Rust 编写的 Windows TUI 小工具，用于把 `.docx` 文档转换为 Markdown。

## 功能
- 两步式交互：先在对话框输入/粘贴 DOCX 路径，再在确认列表回车开始转换
- 支持 `Ctrl+V` 从系统剪贴板粘贴文件路径
- 输出路径自动固定为“原目录 + 同名 `.md`”，无需手动填写
- 支持历史路径（`F2` 打开，回车回填，`Delete` 删除）
- 支持基本段落转换
- 识别 Word 标题样式 `Heading1..Heading6` 映射为 `#..######`
- 识别简单列表（有编号属性的段落）映射为 `- ` 列表项

## 使用说明
1. 双击运行：
   - `F:\codextest\docx2md_tui\target\release\docx2md_tui.exe`
2. 在输入框粘贴 DOCX 路径（`Ctrl+V`）
3. 按 `Enter` 进入确认列表，再按一次 `Enter` 开始转换
4. 转换结果会写到 DOCX 同目录下同名 `.md` 文件

## TUI 快捷键
- `Ctrl+V`：粘贴 DOCX 路径
- `F2`：打开历史路径
- `Enter`：确认/执行
- `q` / `Esc`：退出

## 注意
- 当前版本优先保证稳定和轻量，主要处理文本、标题、简单列表。
- 图片、表格、复杂样式可在后续版本扩展。
