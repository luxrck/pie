"""配置回归测试：上下文窗口 / 输出预留 / 压缩水位（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_config.py
    pytest tests/test_config.py        # 装了 pytest 也能直接跑

覆盖 2026-09 的配置改名（+ 语义修正）：

- `max_tokens` → `reserved_tokens`、`max_seq_len` → `context_window`；
- `context_soft_ratio` / `context_target_ratio` → `[compaction]` 下的 `soft_ratio` / `target_ratio`；
- 两个比例从此相对「可用输入预算 = context_window - reserved_tokens」，而不是整个窗口。

背景（改名的原因）：服务端的超限判定是 `输入 tokens + max_tokens ≤ 窗口`，所以软阈值若按
「整个窗口 × 比例」算，就会在输入还没到水位时先把请求发超限（实测 793,513 输入 + 256,000
预留 = 1,049,513 > 1,048,576 → 400，整个回合被打断）。
"""

from __future__ import annotations

import pathlib
import sys
import tempfile
import textwrap

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))

from pie.config import (  # noqa: E402
    DEFAULT_CONTEXT_WINDOW,
    DEFAULT_RESERVED_TOKENS,
    Config,
    parse_reserved_tokens,
)


def _load_toml(text: str) -> Config:
    """把 TOML 文本写成临时文件后 Config.load（临时目录随 with 结束销毁）。"""
    with tempfile.TemporaryDirectory() as d:
        path = pathlib.Path(d) / "config.toml"
        path.write_text(textwrap.dedent(text), encoding="utf-8")
        return Config.load(config_file=path)


def test_budget_is_window_minus_reserved() -> None:
    """可用输入预算 = context_window - reserved_tokens（不是整个窗口）。"""
    cfg = Config()
    assert cfg.context_window == DEFAULT_CONTEXT_WINDOW
    assert cfg.reserved_tokens == DEFAULT_RESERVED_TOKENS
    assert cfg.context_budget() == cfg.context_window - cfg.reserved_tokens


def test_ratios_are_relative_to_budget() -> None:
    """软阈值 / 目标水位都相对可用输入预算，且必须落在预算之内。"""
    cfg = Config()
    budget = cfg.context_budget()
    assert cfg.soft_limit() == int(budget * 0.8)
    assert cfg.target_limit() == int(budget * 0.55)
    assert cfg.target_limit() < cfg.soft_limit() <= budget


def test_auto_compact_threshold_overrides_soft_limit() -> None:
    """--auto-compact-threshold 仍优先于比例算出的软阈值。"""
    cfg = Config()
    cfg.auto_compact_threshold = 1234
    assert cfg.soft_limit() == 1234


def test_reserved_none_means_no_max_tokens_sent() -> None:
    """reserved_tokens = auto/None → 不发 max_tokens，整个窗口都能装历史。"""
    assert parse_reserved_tokens("auto") is None
    assert parse_reserved_tokens("0") is None
    assert parse_reserved_tokens("") is None
    assert parse_reserved_tokens("64k") == 64_000
    assert parse_reserved_tokens("384K") == 384_000
    cfg = _load_toml('reserved_tokens = "auto"\ncontext_window = 1000\n')
    assert cfg.reserved_tokens is None
    assert cfg.context_budget() == 1000


def test_legacy_keys_migrate() -> None:
    """旧键（max_tokens / max_seq_len / context_*_ratio）在 load 时迁移到新键。"""
    cfg = _load_toml(
        """
        max_tokens = 65536
        max_seq_len = 1048576
        context_soft_ratio = 0.9
        context_target_ratio = 0.5

        [compaction]
        turn = true
        """
    )
    assert cfg.reserved_tokens == 65_536
    assert cfg.context_window == 1_048_576
    assert cfg.soft_ratio == 0.9
    assert cfg.target_ratio == 0.5
    assert cfg.soft_limit() == int((1_048_576 - 65_536) * 0.9)


def test_legacy_ratios_without_compaction_section() -> None:
    """旧配置没有 [compaction] 段时，顶层水位比例也要迁进 compaction。"""
    cfg = _load_toml(
        """
        context_soft_ratio = 0.9
        context_target_ratio = 0.5
        """
    )
    assert cfg.soft_ratio == 0.9
    assert cfg.target_ratio == 0.5


def test_new_keys_win_over_legacy() -> None:
    """同名新旧键并存时以新键为准（避免迁移覆盖用户手写的新值）。"""
    cfg = _load_toml(
        """
        max_seq_len = 1000
        context_window = 2000
        max_tokens = 11
        reserved_tokens = 22
        """
    )
    assert cfg.context_window == 2000
    assert cfg.reserved_tokens == 22


def test_save_round_trip_keeps_auto() -> None:
    """TOML 无 null：reserved_tokens=None 落盘成 "auto"，重新加载仍是 None。"""
    with tempfile.TemporaryDirectory() as d:
        path = pathlib.Path(d) / "config.toml"
        cfg = Config()
        cfg.reserved_tokens = None
        cfg.save(path)
        text = path.read_text(encoding="utf-8")
        assert 'reserved_tokens = "auto"' in text
        assert "max_seq_len" not in text and "max_tokens" not in text
        assert "context_soft_ratio" not in text
        cfg2 = Config.load(config_file=path)
        assert cfg2.reserved_tokens is None


def test_real_server_window_compacts_before_overflow() -> None:
    """回归：实测窗口 1,048,576 / 预留 256,000 下，793,513 的输入必须触发压缩，
    且软阈值 <= 窗口 - 预留（否则又会把请求发到 400）。"""
    cfg = _load_toml(
        """
        reserved_tokens = 256000
        context_window = 1048576

        [compaction]
        turn = true
        soft_ratio = 0.8
        target_ratio = 0.55
        """
    )
    failed_input = 793_513
    assert cfg.soft_limit() <= cfg.context_budget()
    assert failed_input >= cfg.soft_limit()  # 会触发压缩（旧口径下 1,024,000 不会）


def test_usage_report_limit_is_budget() -> None:
    """usage_report 的「/ 分母」是可用输入预算，而不是整个窗口。"""
    from pie.session import Session
    from pie.tools import ToolRegistry

    cfg = _load_toml("reserved_tokens = 256000\ncontext_window = 1048576\n\n[compaction]\n")
    cfg.verbose = False
    session = Session.new(config=cfg, llm=object(), tools=ToolRegistry())
    report = session.usage_report()
    assert f"/ {cfg.context_budget():,} tokens" in report
    assert f"输入预算 {cfg.context_budget():,}" in report
    assert f"上下文窗口 {cfg.context_window:,}" in report


def _main() -> int:
    tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith("test_") and callable(f)]
    failed = []
    for name, func in tests:
        try:
            func()
        except AssertionError as exc:
            failed.append((name, exc))
            print(f"[FAIL] {name}: {exc}")
        else:
            print(f"[PASS] {name}")
    print(f"\n{len(tests) - len(failed)}/{len(tests)} 通过")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(_main())

