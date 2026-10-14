#!/usr/bin/env python3
"""Compare Dart/Flutter extraction against the exact source implementation."""
import functools
import os
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,upstream_tests
import graphify.extract as facade
import graphify.extractors.dart as reference

native=Native('dart');original=reference.extract_dart;cases=0
@functools.wraps(original)
def extract(path):
    global cases
    native.use_working_directory(os.getcwd());expected=original(path);actual=native.result('extract.dart',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    with patch.object(reference,'extract_dart',extract),patch.object(facade,'extract_dart',extract),patch.dict(facade._DISPATCH,{k:extract for k,v in facade._DISPATCH.items() if v is original}):
        tests=upstream_tests('tests/test_dart.py','tests/test_extract.py::test_dart_child_node_ids_are_stem_based')
    rng=random.Random(2026092330)
    forms=['','/* multiline\nblock\n*/\n// next\nclass A {\n void method() {}\n}',
      'class A { String url="https://example.com/a/*x*/"; String raw=""" quotes { ( // string\n} """; }\nvoid z() {}',
      'class A<T extends List<Item>>(int n) extends Base<Pair<A,B>, State> with M<A>, Second implements UI.Widget, C<D> {}',
      '@riverpod\nclass ÅThing {}\n@Riverpod(x:1)\nString value() {}\n@override\nvoid method() {}',
      'mixin M on Base {}\nmixin class C {}\nextension type Id(int id) implements Base {}\nextension on String {}',
      'typedef X<T> = pkg.Y<Item>;\ntypedef Fn = Function;\ntypedef MapX = Map<String,dynamic>;',
      'late final String a;\nfinal (int, String) rec = (1,"a");\nvar User(name: value, age: age) = user;\nvar (a,b)=(1,2);',
      'void route() {\n context.go("/home?id=2&x=a");\n Navigator.pushNamed(context, Routes.page);\ncontext.push(PageRoute());\n}',
      'class A {\n void x() { on<Event>(); emit(State()); yield const Other(); context.read<Bloc>().add(Event()); ref.watch(provider); BlocBuilder<Bloc, State>(); }\n}',
      'void a()=>context.read<Bloc>();\nvoid b();\nvoid c() { ref.read(provider); }',
      '@injectable\n'+('é'*295)+'\nclass Target {}',
      'class Large'+(' '*480)+' extends Base {}\nclass Next {}',
      'class Large'+('é'*490)+' extends Base {}\nclass Next {}',
      'class A {}\nclass A {}\nvoid a() {}\nvoid a() {}',
      'part of "parent.dart";\nclass Child {}\nvoid child() {}',
      'part of "missing.dart";\nclass Child {}',
      'part of "parent";\nclass Child {}',
      'import "https://example.com/a//b";\nexport "pkg://url";\nvoid a() {locator<api.Box<User>>();}',
      'class A { /*comment*/\n void b() { final s = \'\'\' { } \'\'\'; ref.watch(x); }\n}',
      'class café extends B {\n void naïve() {ref.watch(é); on<ἔ>();}\n}',
      'void a() {xx\u0301ref.watch(mark); xx\u00b2ref.watch(number); xx\x1fref.watch(control);}',
      'class A {\n final List<Map<String, State>> a;\n  Future<Result> get() {}\n    void deep() {}\n}',
      'class A;\nvoid b() {ref.watch(x);}\nclass C {void c(){}}']
    events=['on<Event>();','emit(State());','yield const Other();','bloc.add(Event());','context.read<Bloc>().add(Event());','ref.watch(provider);','ref.listen(x);','BlocBuilder<MyBloc, MyState>();','context.watch<Auth>();','Provider.of<Auth>();','context.go("/a?x=1&y=2");','Navigator.pushNamed(context, Route.home);','context.push(ProfilePage());','locator<Repo<User>>();','final s="https://host/path";','final r="{";','final r=""" { hello } """;']
    for _ in range(440):
        blocks=[]
        for _ in range(rng.randrange(1,6)):
            name=rng.choice(['A','Model','Notifier','Thing','Éclair','aThing','X']);kind=rng.choice(['class','mixin','enum','abstract base class','mixin class','extension type'])
            header=kind+' '+name+rng.choice(['','<T>','<A<B>,C>'])+rng.choice(['',' extends Base',' extends Bloc<Event,State>',' with Mixin implements Thing',' implements Interface',' on Base'])
            body='\n'.join(rng.choices(events,k=rng.randrange(8)))
            text=header+' {\n  void method() {\n'+body+'\n  }\n}'
            if rng.random()<.4:text=rng.choice(['@riverpod\n','@injectable\n','@RoutePage()\n','/*header\ncomment*/\n'])+text
            blocks.append(text)
        text='\n'.join(blocks)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        forms.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-dart-') as tmp:
        root=Path(tmp);(root/'parent.dart').write_text('library example;\npart "sample.dart";');path=root/'sample.dart'
        for text in forms:path.write_text(text);extract(path)
        for raw in [b'\xffclass A {}',b'\xef\xbb\xbfclass A {}',b'class A {\x00}\r\nvoid run() {}']:
            path.write_bytes(raw);extract(path)
        extract(root/'missing.dart');extract(root)
        for name in ['folder/sample.dart','a+/test.dart','é/test.DART']:
            path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(forms[3]);extract(path)
        path=root/'sample.dart';path.write_text(forms[15]);saved=os.getcwd()
        try:os.chdir(root);extract(Path('sample.dart'))
        finally:os.chdir(saved)
finally:native.close()
native.receipt('graphify/extractors/dart.py',['src/extractors/dart.rs'],extraction_cases=cases,upstream_tests=tests,
    comparisons='Exact comment/string handling and source lines, part-file redirects, nested generic headers, mixins/interfaces, Riverpod/Bloc connections, annotations/provider generation, typedefs, records/destructuring, methods, routes, imports/exports and generic lookups.',
    adaptations=['The original parser is regex based; this port preserves that behavior. Explicit regex-engine failures propagate instead of returning a partial graph. Offsets are native UTF-8 byte positions internally, with original 300/500-character windows retained.'])
