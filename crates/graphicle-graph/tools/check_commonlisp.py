#!/usr/bin/env python3
"""Qualify the entire Common Lisp extractor, including uncommon defining forms."""
import ast
import functools
import importlib.metadata
import os
import random
import tempfile
import types
import warnings
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from upstream_fixes import oracle_source
import graphify.extract as facade
import graphify.extractors.commonlisp as reference
import tree_sitter_commonlisp
from tree_sitter import Language

# Pinned extractor with the reviewed post-pin fix (PR #3544) applied in memory;
# the pinned module object stays in place for facade dispatch identity.
oracle=types.ModuleType('graphify_oracle_commonlisp');oracle.__package__='graphify.extractors'
exec(compile(oracle_source('graphify/extractors/commonlisp.py'),'<pinned commonlisp.py with reviewed fixes>','exec'),oracle.__dict__)

native=Native('commonlisp');original=oracle.extract_commonlisp;cases=0
@functools.wraps(original)
def extract(path):
    global cases
    native.use_working_directory(os.getcwd());expected=original(path);actual=native.result('extract.commonlisp',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_languages.py::'+n.name for n in ast.parse((SOURCE/'tests/test_languages.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_cl_')]
    with patch.object(reference,'extract_commonlisp',extract),patch.object(facade,'extract_commonlisp',extract),patch.dict(facade._DISPATCH,{k:extract for k,v in facade._DISPATCH.items() if v is reference.extract_commonlisp}):tests=upstream_tests(*selected)
    rng=random.Random(2026092329)
    forms=['','; comment','(defpackage :pkg (:use :cl :alexandria))\n(in-package :pkg)\n(defclass child (unknown) ())\n(defclass unknown () ())',
      '(DEFPACKAGE :PKG (:USE :CL))\n(IN-PACKAGE :PKG)\n(DEFCLASS UPPER () ())\n(DEFUN WORK () (HELP))\n(DEFUN HELP () 1)',
      '(defmethod method :before ((a item) (b (eql 3))) "method docs" (helper))\n(defclass item () ())\n(defun helper () 1)',
      '(defgeneric process (x))\n(defmethod process ((x thing)) (helper))\n(defun helper () 1)',
      '(defun a (x) "Doc" "Second" (b))\n(defun b () (a 1))\n(defun b () (a 2))',
      '(defstruct (thing (:conc-name "X-")) "Documentation" x y)\n(defparameter *text* "value" "actual doc")',
      '(require :pkg)\n(require "other")\n(ql:quickload :foo)\n(ql:quickload "bar")\n(require \'pkg)',
      '(in-package :missing)\n(defun plain () 1)\n(defpackage missing (:use :cl))\n(defun nested () (plain))',
      '#+linux (progn (defun a () (b)) (defun b () 2))\n#-windows (definline foo () (a))',
      '(optimizing (eval-when (:execute) (defun a () 1)))\n(default-value (defun b () (a)))',
      '(defmacro nested () `(progn (a) (b)))\n(defun a () 1)\n(defun b () 2)',
      '(defun =< >) (defun a!%&/ () 1)',
      '(defun café () "'+('é😀'*90)+'" (α))\n(defun α () 1)',
      # PR #3544: define-condition keeps its superclass edges like defclass.
      '(define-condition app-error (error) ())\n(define-condition my-error (app-error) ((code :initarg :code)))\n(define-condition bare () () (:report "x"))\n(define-condition (odd) (error) ())']
    for keyword in sorted(reference._CL_DATA_DEFINERS|reference._CL_MACRO_DEFINERS|reference._CL_NOT_DEFINERS|{'definline','define-gadget','def-macro-x'}):
        for name in ['thing','(thing :option "x")']:
            forms.append(f'({keyword} {name} (x) "Docstring" (helper x))\n(defun helper (x) x)')
    for special in sorted(reference._CL_SPECIAL_FORMS):
        forms.append(f'(defun {special} () 1)\n(defun caller () ({special}) (helper))\n(defun helper () 2)')
    names=['work','helper','worker=','worker<','worker>','*thing*','+value+','foo/bar','foo&bar','name?','name!','café','καλός']
    definitions=['defun','defmethod','defgeneric','defmacro','definline','define-modify-macro','defstruct','deftype','defvar','defparameter','defconstant','define-condition','deflate','deftest']
    for _ in range(400):
        parts=[rng.choice(['','(defpackage :app (:use :cl :other)) (in-package :app)','(in-package :external)','(defpackage :one) (defpackage :two)'])]
        for _ in range(rng.randrange(1,7)):
            typ=rng.choice(definitions);name=rng.choice(names);params=rng.choice(['()','(x y)','((x thing))']);doc=rng.choice(['','"Docstring"','""'])
            body=' '.join('('+rng.choice(names+['let','print','funcall'])+' x)' for _ in range(rng.randrange(4)))
            form=f'({typ} {name} {params} {doc} {body})'
            parts.append(rng.choice([form,'(progn '+form+')','#+feature '+form,'(eval-when (:load-toplevel) '+form+')']))
        text='\n'.join(parts)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        forms.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-commonlisp-') as tmp:
        root=Path(tmp);path=root/'sample.lisp'
        for text in forms:path.write_text(text);extract(path)
        for raw in [b'\xff(defun foo () 1)',b'\xef\xbb\xbf(defun foo () 1)',b'(defun a ()\0 (b))',b'\xc0\xaf']:
            path.write_bytes(raw);extract(path)
        extract(root/'missing.lisp');extract(root)
        for name in ['src/a/sample.lisp','src/b/sample.lisp','system.asd','plus+/code.cl','sample.LSP']:
            path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(forms[4]);extract(path)
        saved=os.getcwd()
        try:os.chdir(root);extract(Path('system.asd'))
        finally:os.chdir(saved)
finally:native.close()
with warnings.catch_warnings():
    warnings.simplefilter('ignore');grammar=Language(tree_sitter_commonlisp.language())
native.receipt('graphify/extractors/commonlisp.py',['src/extractors/commonlisp.rs'],extraction_cases=cases,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-commonlisp'),'rust_crate':'=0.4.1','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version,'source_comparison':'commonlisp-grammar.json'},
    comparisons='Exact path-qualified/operator-preserving IDs, packages and imports, sourceless superclass stubs, all standard and custom definers, docstring/value distinction, wrappers/reader conditionals, specializer edges, call filtering, malformed input and file errors.')
