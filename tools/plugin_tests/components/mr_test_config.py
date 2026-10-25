# Phase 4 parity component: ConfigHelper's getters built on _get_option
# (lists, dicts, choices, templates, paths), values and errors
from __future__ import annotations
import logging


def log(msg):
    logging.info(f"mr_test_config: {msg}")


def now(fn):
    try:
        v = fn()
        return ["ok", v if not hasattr(v, "render") else ["template", v.render({"x": 4})]]
    except Exception as e:
        return ["err", type(e).__name__, str(e)]


class MrTestConfig:
    def __init__(self, config):
        c = config
        out = {
            "list": now(lambda: c.getlist("a_list")),
            "list_sep": now(lambda: c.getlist("csv", separator=",")),
            "list_count_bad": now(lambda: c.getlist("csv", separator=",", count=2)),
            "intlist": now(lambda: c.getintlist("ints", separator=",")),
            "floatlist": now(lambda: c.getfloatlist("floats", separator=",")),
            "intlist_bad": now(lambda: c.getintlist("csv", separator=",")),
            "lists": now(lambda: c.getlists("matrix", list_type=int, separators=("\n", ","))),
            "list_default": now(lambda: c.getlist("nope", ["d"])),
            "list_missing": now(lambda: c.getlist("nope")),
            "dict": now(lambda: c.getdict("a_dict")),
            "dict_int": now(lambda: c.getdict("int_dict", dict_type=int)),
            "dict_bad": now(lambda: c.getdict("bad_dict")),
            "dict_empty_ok": now(lambda: c.getdict("bad_dict", allow_empty_fields=True)),
            "choice": now(lambda: c.getchoice("mode", ["fast", "slow"])),
            "choice_dict": now(lambda: c.getchoice("mode", {"fast": 1, "slow": 2})),
            "choice_bad": now(lambda: c.getchoice("mode", ["a", "b"])),
            "choice_default": now(lambda: c.getchoice("nope", ["x", "y"], "y")),
            "template": now(lambda: c.gettemplate("tmpl")),
            # a syntax error: Jinja2's exception type (the message is the
            # template engine's own; Moonruster's engine is minijinja)
            "load_template_bad": now(lambda: c.load_template("nope", "plain {{ 1 + 1 }}"))[:2],
            "load_template": now(lambda: c.load_template("nope", "plain {1 + 1}")),
            "path": now(lambda: str(c.getpath("a_path"))),
            "bool_via_config": now(lambda: c.config.getboolean(c.section, "flag")),
            "has_option": now(lambda: c.has_option("a_list")),
            "section": now(lambda: [c.section, c.get_name()]),
        }
        for k, v in out.items():
            log(f"{k}: {v!r}")


def load_component(config):
    return MrTestConfig(config)
