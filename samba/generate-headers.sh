#!/usr/bin/env bash
# Prepare only the private headers used by tier.so from configured Debian sources.
set -euo pipefail
cd "${1:?configured Samba source directory}"
build=bin/default
mkdir -p "$build/lib/param" "$build/source3/param" "$build/libcli/util" \
    "$build/librpc/gen_ndr" "$build/source3/librpc/gen_ndr" \
    "$build/docs-xml/smbdotconf"
python3 - <<'PY'
from pathlib import Path
import re
import ast
from xml.sax.saxutils import quoteattr
# Reproduce docs-xml/smbdotconf/wscript_build ordering and path entities.
root = Path('docs-xml/smbdotconf')
articles = sorted(root.rglob('*.xml'), key=lambda path: path.name)
body = ''.join(path.read_text() for path in articles if path.name != 'parameters.all.xml')
config = {}
for line in Path('bin/c4che/default_cache.py').read_text().splitlines():
    key, separator, value = line.partition(' = ')
    if separator:
        config[key] = ast.literal_eval(value)
config['MITKDCPATH'] = config.get('MIT_KDC_PATH', '')
entities = sorted(set(re.findall(r'&pathconfig\.([A-Z_]+);', body)))
dtd = '\n'.join(f'<!ENTITY pathconfig.{name} {quoteattr(str(config[name]))}>' for name in entities)
Path('bin/default/docs-xml/smbdotconf/parameters.all.xml').write_text(
    '<!DOCTYPE section [\n' + dtd + '\n]>\n<section>\n' + body + '</section>\n')
PY
xml="$build/docs-xml/smbdotconf/parameters.all.xml"
python3 script/generate_param.py --file "$xml" --output "$build/lib/param/param_local.h" --mode=PARAMDEFS --scope=LOCAL
python3 script/generate_param.py --file "$xml" --output "$build/source3/param/param_proto.h" --mode=S3PROTO
python3 source4/scripting/bin/gen_hresult.py libcli/util/hresult_err_table.txt \
    "$build/libcli/util/hresult.h" "$build/libcli/util/hresult.c" "$build/libcli/util/py_hresult.c"
python3 source4/scripting/bin/gen_ntstatus.py libcli/util/ntstatus_err_table.txt \
    "$build/libcli/util/ntstatus_gen.h" "$build/libcli/util/nterr_gen.c" \
    "$build/libcli/util/py_ntstatus.c" "$build/libcli/util/ntstatus_gen.rs"
python3 source4/scripting/bin/gen_werror.py libcli/util/werror_err_table.txt \
    "$build/libcli/util/werror_gen.h" "$build/libcli/util/werror_gen.c" \
    "$build/libcli/util/werror_friendly_gen.c" "$build/libcli/util/py_werror.c"
for name in auth file_id krb5pac lsa misc nbt netlogon samr security server_id smb2_lease_struct smb3posix smb_acl; do
    perl pidl/pidl --quiet --includedir=librpc/idl --header --outputdir="$build/librpc/gen_ndr" "librpc/idl/$name.idl"
done
perl pidl/pidl --quiet --includedir=librpc/idl --ndr-parser --outputdir="$build/librpc/gen_ndr" librpc/idl/misc.idl
perl pidl/pidl --quiet --includedir=librpc/idl --header --outputdir="$build/source3/librpc/gen_ndr" source3/librpc/idl/smbXsrv.idl
