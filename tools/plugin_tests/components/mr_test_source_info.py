# Phase 3 parity component: moonraker.utils.source_info as components see it
# (Moonraker's own stdlib functions, and the handlers for read_asset,
# get_asset_path and the importlib_metadata ones)
from __future__ import annotations
import logging
import pathlib
import sys

from ..utils import source_info


class MrTestSourceInfo:
    def __init__(self, config):
        log = logging.info
        log(f"mr_test_source_info: source_path={source_info.source_path()}")
        log(f"package_path={source_info.package_path()} is_git_repo={source_info.is_git_repo()}")
        log(f"find_git_repo={source_info.find_git_repo()} is_dist_package={source_info.is_dist_package()}")
        log(f"package_version={source_info.package_version()!r}")
        log(f"normalize={source_info.normalize_project_name('Moonraker-Timelapse.x')!r}")
        asvc = source_info.read_asset("default_allowed_services")
        log(f"read_asset lines={asvc.splitlines()!r}")
        log(f"read_asset missing={source_info.read_asset('no_such_asset')!r}")
        log(f"get_asset_path={source_info.get_asset_path()}")
        info = source_info.load_distribution_info(pathlib.Path(sys.prefix), "tornado")
        log(f"dist tornado: name={info.metadata['Name']} version={info.metadata['Version']} "
            f"release_info={info.release_info!r} direct_url={info.direct_url_data!r} "
            f"type={type(info).__qualname__}")
        try:
            source_info.load_distribution_info(pathlib.Path(sys.prefix), "no-such-project")
        except Exception as e:
            log(f"dist missing: {type(e).__name__}: {e}")
        log(f"is_vitualenv_project={source_info.is_vitualenv_project()}")


def load_component(config):
    return MrTestSourceInfo(config)
