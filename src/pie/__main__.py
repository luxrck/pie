"""python -m pie 的入口。"""

from .cli import main

__all__: list[str] = []  # 入口模块，没有对外符号（`from pie.__main__ import *` 拿到空）

if __name__ == "__main__":
    raise SystemExit(main())





