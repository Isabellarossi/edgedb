#
# This source file is part of the EdgeDB open source project.
#
# Copyright 2016-present MagicStack Inc. and the EdgeDB authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#


from __future__ import annotations
from typing import *

import enum
import hashlib
import json
import logging
import os
import pathlib
import pickle
import tempfile

import immutables as immu

import edb
from edb.common import devmode

try:
    import pkg_resources
except ImportError:
    pkg_resources = None  # type: ignore

try:
    import setuptools_scm
except ImportError:
    setuptools_scm = None  # type: ignore


class MetadataError(Exception):
    pass


def get_build_metadata_value(prop: str) -> str:
    env_val = os.environ.get(f'_EDGEDB_BUILDMETA_{prop}')
    if env_val:
        return env_val

    try:
        from . import _buildmeta  # type: ignore
        return getattr(_buildmeta, prop)
    except (ImportError, AttributeError):
        raise MetadataError(
            f'could not find {prop} in build metadata') from None


def get_pg_config_path() -> pathlib.Path:
    if devmode.is_in_dev_mode():
        edb_path: os.PathLike = edb.server.__path__[0]  # type: ignore
        root = pathlib.Path(edb_path).parent.parent
        pg_config = (root / 'build' / 'postgres' /
                     'install' / 'bin' / 'pg_config').resolve()
        if not pg_config.is_file():
            try:
                pg_config = pathlib.Path(
                    get_build_metadata_value('PG_CONFIG_PATH'))
            except MetadataError:
                pass

        if not pg_config.is_file():
            raise MetadataError('DEV mode: Could not find PostgreSQL build, '
                                'run `pip install -e .`')

    else:
        pg_config = pathlib.Path(
            get_build_metadata_value('PG_CONFIG_PATH'))

        if not pg_config.is_file():
            raise MetadataError(
                f'invalid pg_config path: {pg_config!r}: file does not exist '
                f'or is not a regular file')

    return pg_config


def get_runstate_path(data_dir: pathlib.Path) -> pathlib.Path:
    if devmode.is_in_dev_mode():
        return data_dir
    else:
        return pathlib.Path(get_build_metadata_value('RUNSTATE_DIR'))


def get_shared_data_dir_path() -> pathlib.Path:
    if devmode.is_in_dev_mode():
        return devmode.get_dev_mode_cache_dir()
    else:
        return pathlib.Path(get_build_metadata_value('SHARED_DATA_DIR'))


def hash_dirs(dirs: Tuple[str, str]) -> bytes:
    def hash_dir(dirname, ext, paths):
        with os.scandir(dirname) as it:
            for entry in it:
                if entry.is_file() and entry.name.endswith(ext):
                    paths.append(entry.path)
                elif entry.is_dir():
                    hash_dir(entry.path, ext, paths)

    paths = []
    for dirname, ext in dirs:
        hash_dir(dirname, ext, paths)

    h = hashlib.sha1()  # sha1 is the fastest one.
    for path in sorted(paths):
        with open(path, 'rb') as f:
            h.update(f.read())

    return h.digest()


def read_data_cache(
    cache_key: bytes,
    path: os.PathLike,
    *,
    pickled: bool=True,
    source_dir: Optional[pathlib.Path] = None,
) -> Any:
    if source_dir is None:
        source_dir = get_shared_data_dir_path()
    full_path = source_dir / path

    if full_path.exists():
        with open(full_path, 'rb') as f:
            src_hash = f.read(len(cache_key))
            if src_hash == cache_key:
                if pickled:
                    data = f.read()
                    try:
                        return pickle.loads(data)
                    except Exception:
                        logging.exception(f'could not unpickle {path}')
                else:
                    return f.read()


def write_data_cache(
    obj: Any,
    cache_key: bytes,
    path: os.PathLike,
    *,
    pickled: bool = True,
    target_dir: Optional[pathlib.Path] = None,
):
    if target_dir is None:
        target_dir = get_shared_data_dir_path()
    full_path = target_dir / path

    try:
        with tempfile.NamedTemporaryFile(
                mode='wb', dir=full_path.parent, delete=False) as f:
            f.write(cache_key)
            if pickled:
                pickle.dump(obj, file=f, protocol=pickle.HIGHEST_PROTOCOL)
            else:
                f.write(obj)
    except Exception:
        try:
            os.unlink(f.name)
        except OSError:
            pass
        finally:
            raise
    else:
        os.rename(f.name, full_path)


class VersionStage(enum.IntEnum):

    DEV = 0
    ALPHA = 10
    BETA = 20
    RC = 30
    FINAL = 40


class Version(NamedTuple):

    major: int
    minor: int
    stage: VersionStage
    stage_no: int
    local: Tuple[str, ...]

    def __str__(self):
        ver = f'{self.major}.{self.minor}'
        if self.stage is not VersionStage.FINAL:
            ver += f'-{self.stage.name.lower()}.{self.stage_no}'
        if self.local:
            ver += f'{("+" + ".".join(self.local)) if self.local else ""}'

        return ver


def parse_version(ver: Any) -> Version:
    v = ver._version
    local = []
    if v.pre:
        if v.pre[0] == 'a':
            stage = VersionStage.ALPHA
        elif v.pre[0] == 'b':
            stage = VersionStage.BETA
        elif v.pre[0] == 'c':
            stage = VersionStage.RC
        else:
            raise MetadataError(
                f'cannot determine release stage from {ver}')

        stage_no = v.pre[1]

        if v.dev:
            local.extend(['dev', str(v.dev[1])])
    elif v.dev:
        stage = VersionStage.DEV
        stage_no = v.dev[1]
    else:
        stage = VersionStage.FINAL
        stage_no = 0

    if v.local:
        local.extend(v.local)

    return Version(
        major=v.release[0],
        minor=v.release[1],
        stage=stage,
        stage_no=stage_no,
        local=tuple(local),
    )


def get_version() -> Version:
    if devmode.is_in_dev_mode():
        if pkg_resources is None:
            raise MetadataError(
                'cannot determine build version: no pkg_resources module')
        if setuptools_scm is None:
            raise MetadataError(
                'cannot determine build version: no setuptools_scm module')
        version = setuptools_scm.get_version(
            root='../..', relative_to=__file__)
        pv = pkg_resources.parse_version(version)
        version = parse_version(pv)
    else:
        vertuple: List[Any] = list(get_build_metadata_value('VERSION'))
        vertuple[2] = VersionStage(vertuple[2])
        version = Version(*vertuple)

    return version


_version_dict: Optional[immu.Map[str, Any]] = None


def get_version_dict() -> immu.Map[str, Any]:
    global _version_dict

    if _version_dict is None:
        ver = get_version()
        _version_dict = immu.Map({
            'major': ver.major,
            'minor': ver.minor,
            'stage': ver.stage.name.lower(),
            'stage_no': ver.stage_no,
            'local': tuple(ver.local) if ver.local else (),
        })

    return _version_dict


_version_json: Optional[str] = None


def get_version_json() -> str:
    global _version_json
    if _version_json is None:
        _version_json = json.dumps(dict(get_version_dict()))
    return _version_json
