#!/usr/bin/env python3
"""Compare every Apex output field and original language regressions."""
import ast
import random
import tempfile
import types
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from upstream_fixes import oracle_source
import graphify.extract as facade
import graphify.extractors.apex as reference

oracle=types.ModuleType("reference_apex");oracle.__package__="graphify.extractors"
exec(compile(oracle_source("graphify/extractors/apex.py"),"<pinned apex.py with reviewed fixes>","exec"),oracle.__dict__)
native=Native("apex");original=oracle.extract_apex;cases=0
def extract(path):
    global cases
    expected=original(path);actual=native.result("extract.apex",path=str(path));cases+=1
    if actual!=expected:native.fail(path=str(path),source=path.read_text(errors="replace") if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=["tests/test_languages.py::"+n.name for n in ast.parse((SOURCE/"tests/test_languages.py").read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith("test_apex_")]
    with patch.object(reference,"extract_apex",extract),patch.object(facade,"extract_apex",extract):tests=upstream_tests(*selected)
    rng=random.Random(2026092316)
    lines=['public class Customer extends Parent implements IFoo, IBar {','global with sharing virtual class Thing {',
           'public interface IFoo extends IOther, IGeneric {','public enum State {ONE, TWO}',
           'trigger Hook on Account (before insert, before update) {','@AuraEnabled','@InvocableMethod(label="Go")','@Other @InvocableMethod()',
           'public static List<Account> fetch() {','private void run() throws Error {','public void if() {','public class null {}',
           'public interface new {}','public enum false {}','}', '','/* comment */', '// insert record;',
           'List<Account> a = [SELECT Id FROM Account];','update records; insert items;', 'upsert myData;',
           'return thing.call();','@AuraEnabled public void inlineSkipped() {}','public class Ω extends Σ implements 变量 {}',
           'publiİ class Dotted {}','publiı class Dotless {}','public ſtatic void run() {}','claſs LongS {}',
           'xinsert record; update row;', '\u0301insert data;', '\U0001e4d0insert data;',
           '[SELECT\u0301 FROM Account]','[SELECT\U0001e4d0 FROM Account]','[SELECT Id FROM A] [SELECT Name FROM B]',
           'interface Alone {}','enum E {}','webService class W {}','public inherited sharing class I {}',
           # PRs #3278/#3276: qualified and generic heritage clauses and return types.
           'public class Batch implements Database.Batchable<sObject>, Schedulable {','public Database.QueryLocator start(Database.BatchableContext bc) { return null; }',
           'public Map<String, Object> mapReturn() { return null; }','public interface IGen extends System.Comparable<Map<String, Id>>, IOther {',
           'public class Sub extends Outer.Base<List<Account>> {','public static List< Account > spaced() {','insert new Account(Name = x);','public Map <String, Id> loose() {']
    with tempfile.TemporaryDirectory(prefix="graphify-apex-") as tmp:
        root=Path(tmp);path=root/"Example.cls"
        for line in lines:
            path.write_text('public class Start {\n'+line+'\npublic void tail() {}');extract(path)
        for _ in range(650):
            text=rng.choice(['\n','\r\n','\r','\u2028']).join(rng.choices(lines,k=rng.randrange(35)))
            if rng.random()<.3:text=text.upper()
            if rng.random()<.2:text=text.replace(' ',rng.choice(['\t','\x1c','\u00a0']))
            if rng.random()<.1:text=text[:rng.randrange(max(len(text),1))]
            path.write_bytes(text.encode()+b'\xff');extract(path)
        for name in ['Résumé.cls','Σ.trigger','space name.cls','dir\\other.cls']:
            path=root/name;path.write_text('\n'.join(lines[:12]));extract(path)
        extract(root/'missing.cls');extract(root)
finally:native.close()
native.receipt("graphify/extractors/apex.py",["src/extractors/apex.rs"],extraction_cases=cases,upstream_tests=tests,
    comparisons="Classes, interfaces, enums, triggers, source attribution, pending annotations, method labels, inheritance, SOQL/DML, comment/inline behavior, Unicode-15 word boundaries and Python IGNORECASE special letters.",
    adaptations=["Static patterns use native Rust regex engines with Python-compatible word/space/case classes. A native regex resource-limit failure is reported explicitly rather than returning partial extraction.","The port intentionally preserves upstream regex semantics, including its lack of brace-aware scope tracking."])
