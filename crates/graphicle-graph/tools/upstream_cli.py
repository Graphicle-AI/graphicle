"""Fixture adapters to keep native substitutions visible to upstream CLI tests.

- `in_process_graphify()` runs the reference CLI in this process for the
  tests' `python -m graphify` children, so functions a check substituted with
  native calls are reached.
- `native_graphify()` runs those children on the native executable under
  test instead: same arguments (renamed flags mapped, tools/naming.py), working
  directory, input and environment, every GRAPHIFY_<X> copied to
  GRAPHICLE_<X> and the reference output folder (layout 'upstream'); the
  child's stdout and stderr are mapped back to the reference names, and a
  '.graphicle' path component anywhere else fails the test.
"""
import contextlib
import io
import os
import subprocess
import sys

def _graphify_child(args):
    return isinstance(args,list) and args[1:3]==["-m","graphify"]

@contextlib.contextmanager
def in_process_graphify():
    original=subprocess.run
    def run(args,*positional,**kwargs):
        if not _graphify_child(args):return original(args,*positional,**kwargs)
        import graphify.__main__ as mainmod
        saved_cwd=os.getcwd();saved_argv=sys.argv;saved_env=dict(os.environ)
        out=io.StringIO();err=io.StringIO();code=0
        try:
            if "env" in kwargs:
                os.environ.clear();os.environ.update(kwargs["env"])
            os.chdir(kwargs.get("cwd",saved_cwd));sys.argv=["graphify",*args[3:]]
            with contextlib.redirect_stdout(out),contextlib.redirect_stderr(err):
                try:mainmod.main()
                except SystemExit as exc:code=exc.code or 0
        finally:
            os.chdir(saved_cwd);sys.argv=saved_argv
            os.environ.clear();os.environ.update(saved_env)
        return subprocess.CompletedProcess(args,code,out.getvalue(),err.getvalue())
    subprocess.run=run
    try:yield
    finally:subprocess.run=original

@contextlib.contextmanager
def native_graphify(counts=None):
    """Run the tests' `python -m graphify ...` children on the native executable.

    `counts`, a dict, receives the number of native runs per subcommand.
    """
    import naming
    from parity import BINARY
    original=subprocess.run
    def translate(value,text):
        if value is None:return None
        if text:return naming.to_upstream(value,'upstream')
        return naming.to_upstream(value.decode('utf-8','surrogateescape'),'upstream').encode('utf-8','surrogateescape')
    def run(args,*positional,**kwargs):
        if not _graphify_child(args):return original(args,*positional,**kwargs)
        argv=[str(BINARY),*[naming.NATIVE_FLAGS.get(str(a),str(a)) for a in args[3:]]]
        check=kwargs.pop("check",False)
        env=naming.native_env(dict(kwargs.get("env") if kwargs.get("env") is not None else os.environ),'upstream')
        kwargs["env"]=env
        result=original(argv,*positional,**kwargs)
        text=bool(kwargs.get("text") or kwargs.get("universal_newlines") or kwargs.get("encoding") or kwargs.get("errors"))
        result=subprocess.CompletedProcess(args,result.returncode,translate(result.stdout,text),translate(result.stderr,text))
        if counts is not None:
            command=str(args[3]) if len(args)>3 else ""
            counts[command]=counts.get(command,0)+1
        if check and result.returncode:
            raise subprocess.CalledProcessError(result.returncode,args,result.stdout,result.stderr)
        return result
    subprocess.run=run
    try:yield
    finally:subprocess.run=original
