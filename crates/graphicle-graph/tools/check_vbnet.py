#!/usr/bin/env python3
"""Check VB.NET extraction and partial-type call resolution against pinned Graphify."""
import importlib.metadata
import random
import tree_sitter_vb_dotnet
import upstream_fixes
import graphify.extractors.vbnet as reference
exec(compile(upstream_fixes.oracle_source('graphify/extractors/vbnet.py'),
             '<pinned vbnet.py with reviewed namespace fix>', 'exec'), reference.__dict__)
from language_harness import Harness
from tree_sitter import Language

harness = Harness('vbnet', 'vbnet', resolver='vbnet_partial_calls')
try:
    tests = harness.upstream('tests/test_vbnet_extractor.py',
                             deselect={'test_vbnet_missing_parser_reports_install_hint'})
    rng = random.Random(2026092704)
    sources = [
        '', 'Imports System\nImports System.Collections.Generic\nImports Alias = System.Text\n',
        'Namespace A.B\n Namespace C\n  Public Class K\n  End Class\n End Namespace\nEnd Namespace\n',
        'Public Class Counter\n Inherits Base.Counter\n Implements IWorker, IOther\n Private count As Integer, total As Long\n'
        ' Public Event Changed As EventHandler\n Public Property Value As Integer\n Public Sub New()\n  Run()\n End Sub\n'
        ' Public Sub Run() Handles Me.Changed, Other.Tick\n  hElPeR() : Me.Helper(1) : MyClass.Helper() : Counter.Helper(1, 2) : other.Helper()\n End Sub\n'
        ' Private Sub Helper(Optional x As Integer = 1, Optional y As Integer = 2)\n End Sub\n'
        ' Private Function Helper(a As Integer) As Integer\n  Return Helper()\n End Function\n'
        ' Class Nested\n  Sub Inner()\n   Run()\n  End Sub\n End Class\nEnd Class\n',
        'Module Tools\n Sub Main()\n  Work()\n End Sub\n Sub Work()\n End Sub\nEnd Module\n',
        'Public Interface IWorker\n Sub Run()\n Function Count() As Integer\nEnd Interface\nPublic Structure Pair\n Public A As Integer\nEnd Structure\nEnum State\n OnValue\n OffValue = 2\nEnd Enum\n',
        'Public Class Broken\n Sub Valid()\n End Sub\n\' Sub Ghost()\n Sub Invalid(\n',
        'Public Class Ω\n Sub π()\n  Café()\n End Sub\n Sub Café()\n End Sub\nEnd Class\n',
        # Type/module-qualified calls resolve to the named owner; value receivers stay unresolved (0.9.73).
        'Namespace App\n Public Class Worker\n  Sub Run()\n   Helpers.Log()\n   Helpers.Log(1)\n   App.Helpers.Log()\n   helpers.LOG()\n'
        '   Missing.Log()\n   value.Log()\n   Other.Ping()\n   Other.Ping(1, 2)\n   Me.Run()\n  End Sub\n End Class\n Module Helpers\n  Sub Log()\n  End Sub\n'
        '  Sub Log(x As Integer)\n  End Sub\n End Module\n Class Other\n  Shared Sub Ping()\n  End Sub\n  Shared Sub Ping()\n  End Sub\n End Class\nEnd Namespace\n',
        'Module Helpers\n Sub Log()\n End Sub\nEnd Module\nNamespace Inner\n Module Helpers\n  Sub Log()\n  End Sub\n End Module\n Class Caller\n  Sub Go()\n   Helpers.Log()\n  End Sub\n End Class\nEnd Namespace\n',
    ]
    names = ['Run', 'Helper', 'Work', 'Tick']
    helpers = ('Namespace Alpha\n Module Helpers\n  Sub Log()\n  End Sub\n End Module\nEnd Namespace\n'
               'Namespace Beta\n Module Helpers\n  Sub Log()\n  End Sub\n End Module\nEnd Namespace\n')
    for namespace in ['', 'Beta', 'Beta.Nested']:
        for receiver in ['Beta.Helpers', 'Helpers', 'Global.Beta.Helpers', 'Missing.Helpers']:
            caller = f'Class Caller\n Sub Run()\n  {receiver}.Log()\n End Sub\nEnd Class\n'
            sources.append(helpers + (f'Namespace {namespace}\n{caller}End Namespace\n' if namespace else caller))
    for _ in range(400):
        members = []
        for _ in range(rng.randrange(1, 6)):
            name = rng.choice(names)
            arity = rng.randrange(3)
            params = ', '.join(f'{rng.choice(["", "Optional ", "ByVal "])}p{i} As Integer' + (' = 1' if rng.random() < .3 else '') for i in range(arity))
            calls = '\n'.join('   ' + c for c in rng.choices([f'{rng.choice(names).lower()}({", ".join(["1"] * rng.randrange(3))})', 'Me.Run()', 'Counter.Work()', 'x.Run()', 'Dim y As Integer = 1',
                                                              'Helpers.Work()', 'helpers.run(1)', 'Helpers.Missing()'], k=rng.randrange(1, 4)))
            members.append(rng.choice([
                f'  Public Sub {name}({params})\n{calls}\n  End Sub',
                f'  Private Function {name}({params}) As Integer\n{calls}\n  End Function',
                '  Private count As Integer', '  Public Event Tick As EventHandler', '  Public Property Value As String',
                f'  Sub OnTick() Handles Me.Tick\n{calls}\n  End Sub', '  Sub New()\n  End Sub',
            ]))
        kind = rng.choice(['Class', 'Module', 'Structure', 'Partial Public Class'])
        end = 'Class' if 'Class' in kind else kind
        text = f' Public {kind} Counter\n' + '\n'.join(members) + f'\n End {end}\n'
        helper = rng.choice(['', ' Module Helpers\n  Sub Work()\n  End Sub\n  Function Run(p0 As Integer) As Integer\n  End Function\n End Module\n',
                             ' Class Helpers\n  Shared Sub Work()\n  End Sub\n End Class\n'])
        text = helper + text if rng.random() < .5 else text + helper
        text = f'Namespace {rng.choice(["Demo", "App.Core"])}\n' + text + 'End Namespace\n'
        if rng.random() < .2:
            text = text[:rng.randrange(len(text))]
        if rng.random() < .1:
            text = text.replace('\n', '\r\n')
        sources.append(text)
    sources += [b'\xff Class A\nEnd Class\n', b'\xef\xbb\xbfClass A\n Sub F()\n End Sub\nEnd Class\n', b'Class A\n Sub F()\n\x00 G()\n End Sub\nEnd Class\n']
    harness.files('.vb', sources, names=['Counter.Designer.vb', 'résumé.vb', 'two spaces.vb', 'dir/nested.vb'])
    corpora = [
        {'Counter.vb': 'Namespace Demo\n Partial Public Class Counter\n  Sub Run()\n   Helper()\n   Missing()\n   Twice(1)\n  End Sub\n End Class\nEnd Namespace\n',
         'Counter.Designer.vb': 'Namespace Demo\n Partial Class Counter\n  Private Sub Helper(Optional x As Integer = 1)\n  End Sub\n  Sub Twice(a As Integer)\n  End Sub\n End Class\nEnd Namespace\n',
         'Other.vb': 'Namespace Demo\n Partial Class Counter\n  Sub Twice(b As Integer)\n  End Sub\n End Class\nEnd Namespace\n'},
        {'A.vb': 'Class A\n Sub Go()\n  Go(1)\n End Sub\nEnd Class\n', 'B.vb': 'Partial Class A\n Sub Go(x As Integer)\n End Sub\nEnd Class\n'},
        {'Worker.vb': 'Namespace Demo\n Class Worker\n  Sub Run()\n   Tools.Log()\n   Tools.Flush(1)\n   Tools.Flush()\n   Remote.Call()\n  End Sub\n End Class\n'
                      ' Partial Class Tools\n  Shared Sub Log()\n  End Sub\n End Class\nEnd Namespace\n',
         'Tools.Designer.vb': 'Namespace Demo\n Partial Class Tools\n  Shared Sub Flush(n As Integer)\n  End Sub\n End Class\nEnd Namespace\n',
         'Remote.vb': 'Namespace Demo\n Class Remote\n  Shared Sub Call()\n  End Sub\n End Class\nEnd Namespace\n'},
    ]
    for corpus in corpora:
        harness.corpus(corpus)
finally:
    harness.close()
grammar = Language(tree_sitter_vb_dotnet.language())
harness.receipt('graphify/extractors/vbnet.py', ['src/extractors/vbnet.rs', 'vendor/tree-sitter-vb-dotnet/SOURCE.json'],
                upstream_tests=tests,
                grammar={'python_package': 'tree-sitter-vb-dotnet ' + importlib.metadata.version('tree-sitter-vb-dotnet'),
                         'rust_crate': 'vendored graphicle-tree-sitter-vb-dotnet (same generated parser)',
                         'node_kinds': grammar.node_kind_count, 'parse_states': grammar.parse_state_count, 'abi': grammar.abi_version},
                comparisons='Exact nodes, edges, raw calls and errors for imports, namespaces, classes/modules/interfaces/structures/enums, inheritance, fields, properties, events, Handles clauses, optional-parameter arities, case-insensitive calls and type/module-qualified calls; the partial-type resolver on every upstream pipeline run and on multi-file corpora.',
                adaptations=['The pinned grammar is compiled into the native binary; the optional-dependency install hint (test_vbnet_missing_parser_reports_install_hint) has no native counterpart.', 'Reviewed 2026-10-02 namespace lookup fixes qualified calls, prefers the caller namespace and rejects ambiguous short names; three exact-context Python hunks are recorded in migration/upstream-review/adaptations.json.'])
