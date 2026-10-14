#!/usr/bin/env python3
"""Compare each pinned registry key and execute its native function pointer."""
import tempfile
from pathlib import Path
from parity import Native
from graphify.extractors import LANGUAGE_EXTRACTORS
native=Native('extractors_registry');cases=0
samples={
'apex':('.cls','public class AccountService { public void run() {} }'),
'bash':('.sh','#!/bin/bash\nhello() { echo hi; }\nhello\n'),
'blade':('.blade.php',"@extends('layout')\n@section('body')\nHello\n@endsection\n"),
'commonlisp':('.lisp','(defun hello (x) (print x))\n'),
'dart':('.dart','class A { void run() {} }\n'),
'delphi_form':('.dfm','object Main: TForm\n  Caption = \'Example\'\nend\n'),
'dm':('.dm','/obj/example\n    proc/run()\n        world << "hello"\n'),
'dmf':('.dmf','window "main"\n\telem "default"\n\t\ttype = MAIN\n'),
'dmi':('.dmi',b'\x89PNG\r\n\x1a\n'),
'dmm':('.dmm','"a" = (/turf/space)\n(1,1,1) = {"\na\n"}\n'),
'elixir':('.ex','defmodule Example do\n def hello(x), do: IO.puts(x)\nend\n'),
'fortran':('.f90','module example\ncontains\nsubroutine hello()\nend subroutine\nend module\n'),
'go':('.go','package main\nfunc greet() {}\nfunc main() { greet() }\n'),
'json':('.json','{"scripts":{"build":"cargo build"},"dependencies":{"example":"1"}}'),
'julia':('.jl','module Example\nfunction greet(x)\n println(x)\nend\nend\n'),
'lazarus_form':('.lfm','object Main: TForm\n  Caption = \'Example\'\nend\n'),
'markdown':('.md','# Heading\n\nA source document.\n'),
'objc':('.m','@interface Example : NSObject\n- (void)run;\n@end\n@implementation Example\n- (void)run {}\n@end\n'),
'pascal':('.pas','unit Example;\ninterface\nprocedure Hello;\nimplementation\nprocedure Hello;\nbegin\nend;\nend.\n'),
'powershell':('.ps1','function Hello { Write-Output "hi" }\nHello\n'),
'powershell_manifest':('.psd1',"@{ RootModule = 'Example.psm1'; FunctionsToExport = @('Hello') }"),
'razor':('.razor','@page "/example"\n<h1>Hello</h1>\n@code { void Run() {} }\n'),
'rust':('.rs','pub struct Example;\nimpl Example { pub fn run(&self) {} }\n'),
'sln':('.sln','Microsoft Visual Studio Solution File, Format Version 12.0\n'),
'sql':('.sql','CREATE TABLE users (id INTEGER PRIMARY KEY);\nCREATE VIEW active AS SELECT * FROM users;\n'),
'terraform':('.tf','resource "aws_s3_bucket" "example" { bucket = "example" }\n'),
'verilog':('.v','module example(input a, output b);\nassign b = a;\nendmodule\n'),
'zig':('.zig','const std = @import("std");\npub fn main() void {}\n'),
'cobol':('.cbl','       IDENTIFICATION DIVISION.\n       PROGRAM-ID. HELLO.\n       PROCEDURE DIVISION.\n       MAIN.\n           PERFORM WORK.\n       WORK.\n           STOP RUN.\n'),
'erlang':('.erl','-module(example).\n-export([run/0]).\nrun() -> helper().\nhelper() -> ok.\n'),
'r':('.R','helper <- function(x) x + 1\nrun <- function(x) helper(x)\n'),
'solidity':('.sol','pragma solidity ^0.8.0;\ncontract Example { function run() public { helper(); } function helper() internal {} }\n'),
'vbnet':('.vb','Public Class Example\n Sub Run()\n  Helper()\n End Sub\n Sub Helper()\n End Sub\nEnd Class\n'),
}
try:
    names=native.result('extractors.registry')
    if names!=list(LANGUAGE_EXTRACTORS):native.fail(expected=list(LANGUAGE_EXTRACTORS),actual=names)
    assert set(samples)==set(names)
    with tempfile.TemporaryDirectory() as temp:
        for language,(extension,source) in samples.items():
            p=Path(temp)/(language+extension);p.write_bytes(source.encode() if isinstance(source,str) else source)
            expected=LANGUAGE_EXTRACTORS[language](p);actual=native.result('extractors.extract',language=language,path=str(p));cases+=1
            if actual!=expected:native.fail(language=language,expected=expected,actual=actual)
    unknown=native.call('extractors.extract',language='unknown',path='/not-read');assert not unknown['ok']
    native.receipt('graphify/extractors/__init__.py',['src/extractors/registry.rs','src/extractors/mod.rs'],registry_entries=len(names),native_dispatch_comparisons=cases,adaptations=['Rust enums unify the fallible/infallible signatures while retaining direct pointers to the translated functions. SQL receives its original default content=None.','Python import-object identity tests do not apply to Rust. Every original registry entry is compared and invoked through the native table. Full extract.py facade is tracked separately.'])
finally:native.close()
