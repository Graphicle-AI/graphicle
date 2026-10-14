#!/usr/bin/env python3
"""Check COBOL extraction against pinned Graphify, fixed and free format."""
import random
from language_harness import Harness

harness = Harness('cobol', 'cobol')
try:
    tests = harness.upstream('tests/test_cobol_extractor.py')
    rng = random.Random(2026092705)
    fixed = lambda lines: ''.join('       ' + line + '\n' for line in lines)
    numbered = lambda lines: ''.join(f'{(i + 1) * 100:06d} ' + line + '\n' for i, line in enumerate(lines))
    sources = [
        '', fixed(['IDENTIFICATION DIVISION.', 'PROGRAM-ID. ONE.', 'PROCEDURE DIVISION.', 'MAIN.', '    PERFORM A THRU Z.', '    PERFORM B THROUGH C.', '    PERFORM D UNTIL X > 1.', '    PERFORM 10 TIMES', '    END-PERFORM.', 'A.', 'Z.', 'B.', 'C.', 'D.']),
        numbered(['IDENTIFICATION DIVISION.', 'PROGRAM-ID. PAYROLL.', 'PROCEDURE DIVISION.', 'MAIN-PARA.', '    PERFORM INIT-PARA.', 'INIT-PARA.', "    DISPLAY 'HELLO'."]),
        '000100*COMMENT LINE\n000200 IDENTIFICATION DIVISION.\n000300 PROGRAM-ID. X.\n000400D    DISPLAY "DEBUG".\n000500/PAGE\n',
        '>>SOURCE FORMAT IS FREE\nPROGRAM-ID. FREE1.\nPROCEDURE DIVISION.\nP1.\n  PERFORM P2 THRU P3.\n  CALL "SUB" USING X.\nP2.\nP3.\n',
        fixed(['PROGRAM-ID. CP.', 'DATA DIVISION.', '01 REC.', '   05 NAME PIC X.', 'COPY "BOOK.cpy".', "COPY 'OTHER'.", 'COPY LIB-X.', 'PROCEDURE DIVISION.', "MAIN. CALL 'SUB1'. CALL VAR-NAME.", '    EXEC SQL', "      CALL 'NO'", '    END-EXEC.']),
        fixed(['PROGRAM-ID. S1.', 'PROCEDURE DIVISION.', "M. DISPLAY 'PERFORM GHOST'. PERFORM REAL.", 'REAL SECTION.', 'R2.', 'END PROGRAM S1.', 'PROGRAM-ID. S2.', 'PROCEDURE DIVISION.', 'M. PERFORM R2.']),
    ]
    words = ['MAIN-PARA', 'WORK-PARA', 'INIT', 'EXIT-PARA', 'A1', 'Z9']
    for _ in range(400):
        lines = ['IDENTIFICATION DIVISION.', f'PROGRAM-ID. {rng.choice(["PROG", "MAINPROG", "SUB-1"])}.']
        if rng.random() < .5:
            lines += ['DATA DIVISION.', 'WORKING-STORAGE SECTION.', f'01 {rng.choice(words)} PIC X(8).', rng.choice(['COPY CUSTOMER.', "COPY 'CUST.cpy'.", '05 FILLER PIC X.'])]
        lines.append('PROCEDURE DIVISION.')
        for _ in range(rng.randrange(1, 6)):
            lines.append(rng.choice(words) + rng.choice(['.', ' SECTION.']))
            for _ in range(rng.randrange(3)):
                lines.append('    ' + rng.choice([f'PERFORM {rng.choice(words)}.', f'PERFORM {rng.choice(words)} THRU {rng.choice(words)}.',
                                               f"CALL '{rng.choice(words)}'.", f"DISPLAY 'PERFORM {rng.choice(words)}'.", 'STOP RUN.', 'GOBACK.']))
        style = rng.random()
        text = numbered(lines) if style < .3 else fixed(lines) if style < .7 else '>>SOURCE FORMAT FREE\n' + '\n'.join(lines) + '\n'
        if rng.random() < .2:
            text = text[:rng.randrange(len(text))]
        if rng.random() < .1:
            text = text.replace('\n', '\r\n')
        sources.append(text)
    sources += [b'\xff       PROGRAM-ID. A.\n', b'\xef\xbb\xbf       PROGRAM-ID. B.\n', b'       PROGRAM-ID. C.\n\x00\n']
    harness.files('.cbl', sources, names=['main.cob', 'book.cpy', 'x.cobol', 'dir/nested.cbl'])
    corpora = [
        {'main.cob': fixed(['PROGRAM-ID. MAINPROG.', 'DATA DIVISION.', 'COPY CUSTOMER.', 'PROCEDURE DIVISION.', "MAIN. CALL 'SUBPROG'. CALL 'MISSING'."]),
         'subprog.cbl': fixed(['PROGRAM-ID. SUBPROG.', 'PROCEDURE DIVISION.', 'ENTRY-PARA.', '    GOBACK.']),
         'CUSTOMER.cpy': fixed(['01  CUSTOMER-RECORD.', '05  CUSTOMER-NAME PIC X(20).'])},
    ]
    for corpus in corpora:
        harness.corpus(corpus)
finally:
    harness.close()
harness.receipt('graphify/extractors/cobol.py', ['src/extractors/cobol.rs'],
                upstream_tests=tests,
                comparisons='Exact nodes, edges and errors for fixed (blank or sequence-numbered) and free format, programs, sections, paragraphs, PERFORM (including THRU/THROUGH ranges and modifiers), CALL literals and variables, COPY books, data items, comments, debug lines and EXEC blocks; complete multi-file pipeline runs.')
