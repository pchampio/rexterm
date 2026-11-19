#!/usr/bin/env python3
"""Install the server-side Lua companion without discarding existing Rex config."""
import fcntl,argparse,base64,json,os,pathlib,pwd,shlex,shutil,subprocess,sys,tempfile,time
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--endpoint',required=True)
parser.add_argument('--rex',default='rex')
parser.add_argument('--config',help='Explicit server config, when process inspection is unavailable')
parser.add_argument('--source',help=argparse.SUPPRESS)
parser.add_argument('--editor-source',help=argparse.SUPPRESS)
parser.add_argument('--splits-source',help=argparse.SUPPRESS)
parser.add_argument('--editor-only',action='store_true',help='Install only the conditional Neovim Lua integration')
parser.add_argument('--skip-editor',action='store_true',help='Do not modify Neovim configuration')
parser.add_argument('--editor-dir',help='Override the Neovim plugin directory')
parser.add_argument('--ssh');parser.add_argument('--ssh-config');parser.add_argument('--remote-rex',default='rex')
args=parser.parse_args()
source=base64.b64decode(args.source) if args.source else (pathlib.Path(__file__).resolve().parent.parent/'config/rex/server.lua').read_bytes()
editor_source=base64.b64decode(args.editor_source) if args.editor_source else (pathlib.Path(__file__).resolve().parent.parent/'config/rex/nvim.lua').read_bytes()
splits_source=base64.b64decode(args.splits_source) if args.splits_source else (pathlib.Path(__file__).resolve().parent.parent/'config/rex/nvim-smart-splits.lua').read_bytes()
if args.ssh:
    command=['python3','-','--endpoint',args.endpoint,'--rex',args.remote_rex,'--source',base64.b64encode(source).decode(),'--editor-source',base64.b64encode(editor_source).decode(),'--splits-source',base64.b64encode(splits_source).decode()]
    if args.config:command+=['--config',args.config]
    if args.editor_only:command+=['--editor-only']
    if args.skip_editor:command+=['--skip-editor']
    if args.editor_dir:command+=['--editor-dir',args.editor_dir]
    ssh=['ssh']+(['-F',args.ssh_config] if args.ssh_config else [])+['--',args.ssh,shlex.join(command)]
    raise SystemExit(subprocess.run(ssh,input=pathlib.Path(__file__).read_bytes()).returncode)
def install_editor():
    if args.skip_editor:return None
    directory=pathlib.Path(args.editor_dir) if args.editor_dir else pathlib.Path(os.getenv('XDG_CONFIG_HOME') or str(pathlib.Path.home()/'.config'))/'nvim/plugin'
    directory.mkdir(parents=True,exist_ok=True)
    def install_file(target,contents):
        target.parent.mkdir(parents=True,exist_ok=True)
        if target.exists() and target.read_bytes()==contents:return
        if target.exists():shutil.copy2(target,target.with_name(target.name+'.before-rex-'+str(time.time_ns())))
        fd,temporary=tempfile.mkstemp(prefix='.wezterm-rex-',dir=target.parent)
        try:
            with os.fdopen(fd,'wb') as file:file.write(contents);file.flush();os.fsync(file.fileno())
            os.replace(temporary,target)
        finally:
            if os.path.exists(temporary):os.unlink(temporary)
    install_file(directory.parent/'lua/rex_splits.lua',splits_source)
    target=directory/'wezterm-rex.lua'
    install_file(target,editor_source)
    return str(target)

if args.editor_only:
    print(json.dumps({'editor':install_editor()}));raise SystemExit(0)
assert args.endpoint.startswith('unix://'),'Install locally on the Rex server, or use --ssh'
rex=[args.rex,'-S',args.endpoint,'--autostart=false']
def run(command):
    result=subprocess.run(command,capture_output=True,text=True,timeout=30)
    if result.returncode:raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout)
def api(method):return run(rex+['api','call',method,'{}'])
status=api('server.status')
if args.config:config=pathlib.Path(args.config).expanduser()
else:
    proc=pathlib.Path('/proc')/str(status['pid'])
    assert proc.stat().st_uid==os.getuid(),'Rex must run as this user; specify --config otherwise'
    argv=proc.joinpath('cmdline').read_bytes().split(b'\0')
    config=None
    for i,arg in enumerate(argv):
        if arg==b'--config':config=pathlib.Path(os.fsdecode(argv[i+1]));break
        if arg.startswith(b'--config='):config=pathlib.Path(os.fsdecode(arg[9:]));break
    if config is None:
        allowed={b'REX_CONFIG',b'XDG_CONFIG_HOME',b'HOME'}
        env={os.fsdecode(k):os.fsdecode(v) for entry in proc.joinpath('environ').read_bytes().split(b'\0') if b'=' in entry for k,v in [entry.split(b'=',1)] if k in allowed}
        config=pathlib.Path(env.get('REX_CONFIG') or str(pathlib.Path(env.get('XDG_CONFIG_HOME') or str(pathlib.Path(env.get('HOME') or pwd.getpwuid(os.getuid()).pw_dir)/'.config'))/'rex/init.lua'))
    elif not config.is_absolute():config=pathlib.Path(os.readlink(proc/'cwd'))/config
config=config.resolve();config.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
managed=config.parent/'.wezterm-rex';managed.mkdir(exist_ok=True,mode=0o700)
lock=open(managed/'.install.lock','a');fcntl.flock(lock,fcntl.LOCK_EX)
helper=managed/'companion.lua';old_helper=helper.read_bytes() if helper.exists() else None
old=config.read_bytes() if config.exists() else None
begin=b'-- BEGIN WezTerm Rex companion\n';end=b'-- END WezTerm Rex companion\n'
original=old or b''
if begin in original:
    a=original.index(begin);b=original.index(end,a)+len(end);original=original[:a]+original[b:]
include=begin+('dofile('+json.dumps(str(helper),ensure_ascii=False)+')\n').encode()+end
candidate=include+original
backup=None
changed=old_helper!=source or old!=candidate

def atomic(path,data,mode=0o600):
    fd,name=tempfile.mkstemp(prefix=path.name+'.',dir=path.parent)
    try:
        with os.fdopen(fd,'wb') as file:file.write(data);file.flush();os.fsync(file.fileno())
        os.chmod(name,mode);os.replace(name,path)
    finally:
        if os.path.exists(name):os.unlink(name)
def valid(report):
    errors=[d.get('message','') for d in report.get('diagnostics',[]) if d.get('severity')=='error']
    if errors:raise RuntimeError('; '.join(errors))
try:
    atomic(helper,source)
    fd,temporary=tempfile.mkstemp(prefix='.wezterm-rex-check-',suffix='.lua',dir=config.parent)
    with os.fdopen(fd,'wb') as file:file.write(candidate)
    try:valid(run([args.rex,'config','check',temporary,'--json']))
    finally:os.unlink(temporary)
    if old!=candidate:
        if old is not None:
            backup=config.with_name(config.name+'.before-rex-'+str(time.time_ns()));atomic(backup,old)
        mode=config.stat().st_mode&0o777 if config.exists() else 0o600
        atomic(config,candidate,mode)
    if changed:valid(api('server.config_reload'))
except Exception:
    if old is None:config.unlink(missing_ok=True)
    else:atomic(config,old)
    if old_helper is None:helper.unlink(missing_ok=True)
    else:atomic(helper,old_helper)
    try:api('server.config_reload')
    except Exception:pass
    raise
print(json.dumps({'config':str(config),'companion':str(helper),'backup':str(backup) if backup else None,'reloaded':changed,'editor':install_editor()}))
