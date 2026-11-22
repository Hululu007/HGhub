# v1-only 测试用的 import 拦截器（sitecustomize 自动加载）。
#
# iOS v1 包里没有 lxml / pyquery / pycryptodome（二进制包，需交叉编译）。
# 本机测试 venv 里这些包是装着的，用这个拦截器模拟 v1 环境：
# 任何对这些顶级模块的 import 都报 ImportError，验证边车在缺包时
# 仍能启动、加载蜘蛛，并在 base.spider.html() 里报明错而不是静默出错。
import importlib.abc
import importlib.machinery
import sys

_BLOCKED = {"lxml", "pyquery", "Crypto", "Cryptodome"}


class _RaisingLoader(importlib.abc.Loader):
    def create_module(self, spec):
        return None

    def exec_module(self, module):
        raise ImportError(
            f"v1-only test: {module.__name__!r} is not bundled in Python runtime v1"
        )


class _V1Blocker(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path=None, target=None):
        if name.split(".")[0] in _BLOCKED:
            return importlib.machinery.ModuleSpec(name, _RaisingLoader())
        return None


sys.meta_path.insert(0, _V1Blocker())
