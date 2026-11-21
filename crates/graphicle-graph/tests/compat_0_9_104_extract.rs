// Adapted from Graphify 0.9.104, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_solidity_extractor.py, tests/test_csharp_member_nodes.py,
// tests/test_csharp_object_creation.py, tests/test_csharp_underscore_collisions.py,
// tests/test_yaml_extraction.py
//! Extraction and resolution changes ported from Graphify 0.9.104. Cases
//! follow the upstream tests named in each comment.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        graphicle::rcfile::set_language_overrides(None);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    fn run(&self, paths: &[PathBuf]) -> Value {
        extract(
            paths,
            &ExtractOptions {
                root: Some(self.root.clone()),
                cache_root: Some(self.root.join("graphify-out")),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn extract_files(&self, files: &[(&str, &str)]) -> Value {
        let mut paths = files
            .iter()
            .map(|(rel, text)| self.write(rel, text))
            .collect::<Vec<_>>();
        paths.sort();
        self.run(&paths)
    }
}

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}
fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}
fn node<'a>(result: &'a Value, id: &Value) -> Option<&'a Value> {
    nodes(result).iter().find(|n| n["id"] == *id)
}
fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").to_owned()
}
fn file_name(v: &Value) -> String {
    Path::new(v.as_str().unwrap_or(""))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
fn strs(items: &[&[&str]]) -> BTreeSet<Vec<String>> {
    items
        .iter()
        .map(|row| row.iter().map(|s| s.to_string()).collect())
        .collect()
}

// --- Solidity (#5027, #5060, #5062) ------------------------------------------

/// `_function_calls`: (caller, target, target file name) for `calls` edges
/// into a function.
fn sol_function_calls(r: &Value) -> BTreeSet<Vec<String>> {
    edges(r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| {
            let (s, t) = (node(r, &e["source"])?, node(r, &e["target"])?);
            (t["metadata"]["kind"] == "function").then(|| {
                vec![
                    text(&s["label"]),
                    text(&t["label"]),
                    file_name(&t["source_file"]),
                ]
            })
        })
        .collect()
}
/// `_calls_by_line`: (caller, caller line, target, target line) for every
/// `calls` edge.
fn calls_by_line(r: &Value) -> BTreeSet<Vec<String>> {
    edges(r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| {
            let (s, t) = (node(r, &e["source"])?, node(r, &e["target"])?);
            Some(vec![
                text(&s["label"]),
                text(&s["source_location"]),
                text(&t["label"]),
                text(&t["source_location"]),
            ])
        })
        .collect()
}

#[test]
fn solidity_underscore_salt_changes_only_the_clashing_id() {
    // test_solidity_extractor.py test_solidity_underscore_salt_changes_only_the_clashing_id
    const ROLES: &str = "pragma solidity ^0.8.20;\ncontract Roles {\n    event RoleGranted(bytes32 role, address account);\n    function hasRole(bytes32 role, address account) public view returns (bool) { return false; }\n    function grantRole(bytes32 role, address account) public { _grantRole(role, account); }\n    function _grantRole(bytes32 role, address account) internal returns (bool) {\n        if (hasRole(role, account)) return false;\n        emit RoleGranted(role, account);\n        return true;\n    }\n}\n";
    let f = Fixture::new();
    let r = f.extract_files(&[("Roles.sol", ROLES)]);
    let id = |label: &str| {
        nodes(&r)
            .iter()
            .find(|n| n["label"] == label)
            .map(|n| text(&n["id"]))
            .unwrap_or_else(|| panic!("{label}: {r:#}"))
    };
    let contract = id("Roles");
    assert_eq!(
        id("grantRole()"),
        format!("{contract}_function_grantrole_2_bytes32_address")
    );
    assert_eq!(
        id("hasRole()"),
        format!("{contract}_function_hasrole_2_bytes32_address")
    );
    assert_eq!(
        id("RoleGranted"),
        format!("{contract}_event_rolegranted_rolegranted")
    );
    assert!(
        id("_grantRole()")
            .starts_with(&format!("{contract}_function_grantrole_2_bytes32_address_"))
    );
}

#[test]
fn solidity_using_for_binds_the_overload_whose_first_parameter_takes_the_receiver() {
    // test_solidity_extractor.py test_solidity_using_for_binds_a_library_call_on_a_typed_value
    // and test_solidity_using_for_binds_on_a_library_struct_through_a_mapping_and_a_base.
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("SafeCast.sol", "pragma solidity ^0.8.20;\nlibrary SafeCast {\n    function toUint48(uint256 value) internal pure returns (uint48) { return uint48(value); }\n    function toUint48(int256 value) internal pure returns (uint48) { return uint48(uint256(value)); }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {SafeCast} from \"./SafeCast.sol\";\ncontract Vault {\n    using SafeCast for uint256;\n    uint48 public at;\n    function touch(uint x) external { at = x.toUint48(); }\n}\n"),
    ]);
    let target = edges(&r)
        .iter()
        .find(|e| e["relation"] == "calls")
        .map(|e| e["target"].clone())
        .unwrap_or_else(|| panic!("{r:#}"));
    assert_eq!(
        node(&r, &target).unwrap()["metadata"]["first_param_type"],
        "uint256"
    );
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("EnumerableSet.sol", "pragma solidity ^0.8.20;\nlibrary EnumerableSet {\n    struct AddressSet { address[] values; }\n    struct UintSet { uint256[] values; }\n    function add(AddressSet storage set, address value) internal returns (bool) { return true; }\n    function add(UintSet storage set, uint256 value) internal returns (bool) { return true; }\n}\n"),
        ("Base.sol", "pragma solidity ^0.8.20;\nimport {EnumerableSet} from \"./EnumerableSet.sol\";\nabstract contract Base { EnumerableSet.AddressSet internal _inherited; }\n"),
        ("Roles.sol", "pragma solidity ^0.8.20;\nimport {EnumerableSet} from \"./EnumerableSet.sol\";\nimport {Base} from \"./Base.sol\";\ncontract Roles is Base {\n    using EnumerableSet for EnumerableSet.AddressSet;\n    EnumerableSet.AddressSet private _set;\n    mapping(bytes32 => EnumerableSet.AddressSet) private _members;\n    function direct(address a) external { _set.add(a); }\n    function element(bytes32 r, address a) external { _members[r].add(a); }\n    function inherited(address a) external { _inherited.add(a); }\n}\n"),
    ]);
    let lines = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| text(&node(&r, &e["target"]).unwrap()["source_location"]))
        .collect::<BTreeSet<_>>();
    // add(AddressSet, ..), not add(UintSet, ..)
    assert_eq!(lines, BTreeSet::from(["L5".to_string()]), "{r:#}");
}

#[test]
fn solidity_underscore_member_gets_its_own_node_and_calls() {
    // test_solidity_extractor.py test_solidity_underscore_member_gets_its_own_node_and_calls
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Roles.sol", "pragma solidity ^0.8.20;\ncontract Roles {\n    event RoleGranted(bytes32 role, address account);\n    function hasRole(bytes32 role, address account) public view returns (bool) { return false; }\n    function grantRole(bytes32 role, address account) public { _grantRole(role, account); }\n    function _grantRole(bytes32 role, address account) internal returns (bool) {\n        if (hasRole(role, account)) return false;\n        emit RoleGranted(role, account);\n        return true;\n    }\n}\n"),
    ]);
    assert_eq!(
        calls_by_line(&r),
        strs(&[
            &["_grantRole()", "L6", "RoleGranted", "L3"],
            &["_grantRole()", "L6", "hasRole()", "L4"],
            &["grantRole()", "L5", "_grantRole()", "L6"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_underscore_free_function_and_inherited_call_land_right() {
    // test_solidity_extractor.py test_solidity_underscore_free_function_and_inherited_call_land_right
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Base.sol", "pragma solidity ^0.8.20;\nfunction _scale(uint256 x) pure returns (uint256) { return x * 2; }\nfunction scale(uint256 x) pure returns (uint256) { return _scale(x); }\nabstract contract Base {\n    function mint(address to) public { _mint(to); }\n    function _mint(address to) internal {}\n}\n"),
        ("Token.sol", "pragma solidity ^0.8.20;\nimport {Base} from \"./Base.sol\";\ncontract Token is Base {\n    function airdrop(address to) external { _mint(to); }\n}\n"),
    ]);
    assert_eq!(
        calls_by_line(&r),
        strs(&[
            &["airdrop()", "L4", "_mint()", "L6"],
            &["mint()", "L5", "_mint()", "L6"],
            &["scale()", "L3", "_scale()", "L2"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_call_on_a_typed_state_variable_binds_the_interface_method() {
    // test_solidity_extractor.py test_solidity_call_on_a_typed_state_variable_binds_the_interface_method
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("IERC20.sol", "pragma solidity ^0.8.20;\ninterface IERC20 {\n    function transfer(address to, uint256 value) external returns (bool);\n    function balanceOf(address who) external view returns (uint256);\n}\n"),
        ("Token.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Token is IERC20 {\n    function transfer(address to, uint256 value) external returns (bool) { return true; }\n    function balanceOf(address who) external view returns (uint256) { return 0; }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Vault {\n    IERC20 public token;\n    function pay(address to, uint256 v) external { token.transfer(to, v); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[&["pay()", "transfer()", "IERC20.sol"],]),
        "{r:#}"
    );
}

#[test]
fn solidity_call_on_a_typed_parameter_local_or_return_variable_binds() {
    // test_solidity_extractor.py test_solidity_call_on_a_typed_parameter_local_or_return_variable_binds
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("IERC20.sol", "pragma solidity ^0.8.20;\ninterface IERC20 {\n    function transfer(address to, uint256 value) external returns (bool);\n    function balanceOf(address who) external view returns (uint256);\n}\n"),
        ("Token.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Token is IERC20 {\n    function transfer(address to, uint256 value) external returns (bool) { return true; }\n    function balanceOf(address who) external view returns (uint256) { return 0; }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Vault {\n    function viaParam(IERC20 t, address to) external { t.transfer(to, 1); }\n    function viaLocal(address a) external view returns (uint256) {\n        IERC20 local = IERC20(a);\n        return local.balanceOf(a);\n    }\n    function viaReturn(address a) external returns (IERC20 out) {\n        out = IERC20(a);\n        out.transfer(a, 2);\n    }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[
            &["viaLocal()", "balanceOf()", "IERC20.sol"],
            &["viaParam()", "transfer()", "IERC20.sol"],
            &["viaReturn()", "transfer()", "IERC20.sol"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_call_on_a_type_conversion_binds_through_the_base_contract() {
    // test_solidity_extractor.py test_solidity_call_on_a_type_conversion_binds_through_the_base_contract
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Base.sol", "pragma solidity ^0.8.20;\nabstract contract OwnableBase { function owner() public view returns (address) { return address(0); } }\nabstract contract Ownable is OwnableBase {}\n"),
        ("IERC20.sol", "pragma solidity ^0.8.20;\ninterface IERC20 {\n    function transfer(address to, uint256 value) external returns (bool);\n    function balanceOf(address who) external view returns (uint256);\n}\n"),
        ("Token.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Token is IERC20 {\n    function transfer(address to, uint256 value) external returns (bool) { return true; }\n    function balanceOf(address who) external view returns (uint256) { return 0; }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\nimport {Ownable} from \"./Base.sol\";\ncontract Vault {\n    function pay(address a) external { IERC20(a).transfer(a, 1); }\n    function ownerOf(address x) external view returns (address) { return Ownable(x).owner(); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[
            &["ownerOf()", "owner()", "Base.sol"],
            &["pay()", "transfer()", "IERC20.sol"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_using_for_binds_a_library_call_on_a_typed_value() {
    // test_solidity_extractor.py test_solidity_using_for_binds_a_library_call_on_a_typed_value
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("SafeCast.sol", "pragma solidity ^0.8.20;\nlibrary SafeCast {\n    function toUint48(uint256 value) internal pure returns (uint48) { return uint48(value); }\n    function toUint48(int256 value) internal pure returns (uint48) { return uint48(uint256(value)); }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {SafeCast} from \"./SafeCast.sol\";\ncontract Vault {\n    using SafeCast for uint256;\n    uint48 public at;\n    function touch(uint x) external { at = x.toUint48(); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[&["touch()", "toUint48()", "SafeCast.sol"],]),
        "{r:#}"
    );
}

#[test]
fn solidity_using_for_binds_on_a_library_struct_through_a_mapping_and_a_base() {
    // test_solidity_extractor.py test_solidity_using_for_binds_on_a_library_struct_through_a_mapping_and_a_base
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Base.sol", "pragma solidity ^0.8.20;\nimport {EnumerableSet} from \"./EnumerableSet.sol\";\nabstract contract Base { EnumerableSet.AddressSet internal _inherited; }\n"),
        ("EnumerableSet.sol", "pragma solidity ^0.8.20;\nlibrary EnumerableSet {\n    struct AddressSet { address[] values; }\n    struct UintSet { uint256[] values; }\n    function add(AddressSet storage set, address value) internal returns (bool) { return true; }\n    function add(UintSet storage set, uint256 value) internal returns (bool) { return true; }\n}\n"),
        ("Roles.sol", "pragma solidity ^0.8.20;\nimport {EnumerableSet} from \"./EnumerableSet.sol\";\nimport {Base} from \"./Base.sol\";\ncontract Roles is Base {\n    using EnumerableSet for EnumerableSet.AddressSet;\n    EnumerableSet.AddressSet private _set;\n    mapping(bytes32 => EnumerableSet.AddressSet) private _members;\n    function direct(address a) external { _set.add(a); }\n    function element(bytes32 r, address a) external { _members[r].add(a); }\n    function inherited(address a) external { _inherited.add(a); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[
            &["direct()", "add()", "EnumerableSet.sol"],
            &["element()", "add()", "EnumerableSet.sol"],
            &["inherited()", "add()", "EnumerableSet.sol"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_using_for_star_and_file_level_directives_bind() {
    // test_solidity_extractor.py test_solidity_using_for_star_and_file_level_directives_bind
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("SafeCast.sol", "pragma solidity ^0.8.20;\nlibrary SafeCast {\n    function toUint48(uint256 value) internal pure returns (uint48) { return uint48(value); }\n    function toUint48(int256 value) internal pure returns (uint48) { return uint48(uint256(value)); }\n}\n"),
        ("Time.sol", "pragma solidity ^0.8.20;\nlibrary Time {\n    type Delay is uint112;\n    function get(Delay self) internal pure returns (uint32) { return 0; }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {SafeCast} from \"./SafeCast.sol\";\nimport {Time} from \"./Time.sol\";\nusing SafeCast for uint256;\nfunction free(uint256 x) pure returns (uint48) { return x.toUint48(); }\ncontract Vault {\n    using Time for *;\n    Time.Delay private _delay;\n    function cast(uint256 x) external pure returns (uint48) { return x.toUint48(); }\n    function delay() external view returns (uint32) { return _delay.get(); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[
            &["cast()", "toUint48()", "SafeCast.sol"],
            &["delay()", "get()", "Time.sol"],
            &["free()", "toUint48()", "SafeCast.sol"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_typed_receiver_through_a_reexport_a_base_state_variable_or_a_getter() {
    // test_solidity_extractor.py test_solidity_typed_receiver_through_a_reexport_a_base_state_variable_or_a_getter
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Base.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\nabstract contract Base {\n    IERC20 internal constant hevm = IERC20(address(1));\n    function token() public view returns (IERC20) { return hevm; }\n}\n"),
        ("IERC20.sol", "pragma solidity ^0.8.20;\ninterface IERC20 {\n    function transfer(address to, uint256 value) external returns (bool);\n    function balanceOf(address who) external view returns (uint256);\n}\n"),
        ("Token.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Token is IERC20 {\n    function transfer(address to, uint256 value) external returns (bool) { return true; }\n    function balanceOf(address who) external view returns (uint256) { return 0; }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {Base, IERC20} from \"./Base.sol\";\ncontract Vault is Base {\n    function viaBase(address a) external { hevm.transfer(a, 1); }\n    function viaResult(address a) external view returns (uint256) { return token().balanceOf(a); }\n    function viaReexport(address a) external { IERC20(a).transfer(a, 3); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[
            &["viaBase()", "transfer()", "IERC20.sol"],
            &["viaReexport()", "transfer()", "IERC20.sol"],
            &["viaResult()", "balanceOf()", "IERC20.sol"],
            &["viaResult()", "token()", "Base.sol"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_using_function_set_and_global_directive_bind() {
    // test_solidity_extractor.py test_solidity_using_function_set_and_global_directive_bind
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Math.sol", "pragma solidity ^0.8.20;\nlibrary L { function twice(uint256 x) internal pure returns (uint256) { return 2 * x; } }\nfunction half(uint256 x) pure returns (uint256) { return x / 2; }\n"),
        ("Price.sol", "pragma solidity ^0.8.20;\ntype Price is uint128;\nusing {add, eq as ==} for Price global;\nfunction add(Price a, Price b) pure returns (Price) { return a; }\nfunction eq(Price a, Price b) pure returns (bool) { return true; }\n"),
        ("Shop.sol", "pragma solidity ^0.8.20;\nimport {Price} from \"./Price.sol\";\nimport {L, half} from \"./Math.sol\";\ncontract Shop {\n    using {L.twice, half} for uint256;\n    function total(Price a, Price b) external pure returns (Price) { return a.add(b); }\n    function scale(uint256 x) external pure returns (uint256) { return x.twice().half() + x.half(); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[
            &["scale()", "half()", "Math.sol"],
            &["scale()", "twice()", "Math.sol"],
            &["total()", "add()", "Price.sol"],
        ]),
        "{r:#}"
    );
}

#[test]
fn solidity_typed_receiver_fails_closed_when_ambiguous_or_untyped() {
    // test_solidity_extractor.py test_solidity_typed_receiver_fails_closed_when_ambiguous_or_untyped
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("IERC20.sol", "pragma solidity ^0.8.20;\ninterface IERC20 {\n    function transfer(address to, uint256 value) external returns (bool);\n    function balanceOf(address who) external view returns (uint256);\n}\n"),
        ("Mixed.sol", "pragma solidity ^0.8.20;\nimport \"./IERC20.sol\";\nimport \"./Other.sol\";\ncontract Mixed { function m(IERC20 t) external { t.transfer(address(0), 1); } }\n"),
        ("Other.sol", "pragma solidity ^0.8.20;\ninterface IERC20 { function transfer(address to, uint256 value) external returns (bool); }\n"),
        ("Over.sol", "pragma solidity ^0.8.20;\ninterface IOver {\n    function f(uint256 a) external;\n    function f(bytes32 a) external;\n    function g(uint256 a) external;\n}\ncontract Getter { uint256 public total; }\ncontract GetterChild is Getter {}\n"),
        ("Token.sol", "pragma solidity ^0.8.20;\nimport {IERC20} from \"./IERC20.sol\";\ncontract Token is IERC20 {\n    function transfer(address to, uint256 value) external returns (bool) { return true; }\n    function balanceOf(address who) external view returns (uint256) { return 0; }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {IOver, GetterChild} from \"./Over.sol\";\ncontract Vault {\n    function run(IOver o, GetterChild c, uint256 x) external {\n        o.f(x);\n        o.g(x, x);\n        c.total();\n        unknown.g(x);\n        x.g(x);\n    }\n    function ok(IOver o) external { o.g(1); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[&["ok()", "g()", "Over.sol"],]),
        "{r:#}"
    );
}

#[test]
fn solidity_using_for_fails_closed_when_unknown_ambiguous_or_out_of_scope() {
    // test_solidity_extractor.py test_solidity_using_for_fails_closed_when_unknown_ambiguous_or_out_of_scope
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Dup.sol", "pragma solidity ^0.8.20;\nlibrary Dup { function toUint48(uint256 value) internal pure returns (uint48) { return 0; } }\n"),
        ("SafeCast.sol", "pragma solidity ^0.8.20;\nlibrary SafeCast {\n    function toUint48(uint256 value) internal pure returns (uint48) { return uint48(value); }\n    function toUint48(int256 value) internal pure returns (uint48) { return uint48(uint256(value)); }\n}\n"),
        ("Vault.sol", "pragma solidity ^0.8.20;\nimport {SafeCast} from \"./SafeCast.sol\";\nimport {Dup} from \"./Dup.sol\";\ncontract Other {\n    using SafeCast for uint256;\n}\ncontract Vault is Other {\n    using SafeCast for int256;\n    function run(uint256 x, int256 i, uint64 small) external {\n        x.toUint48();\n        small.toUint48();\n        unknown.toUint48();\n    }\n    function ok(int256 i) external pure returns (uint48) { return i.toUint48(); }\n}\ncontract Both {\n    using SafeCast for uint256;\n    using Dup for uint256;\n    function two(uint256 x) external pure { x.toUint48(); }\n}\n"),
    ]);
    assert_eq!(
        sol_function_calls(&r),
        strs(&[&["ok()", "toUint48()", "SafeCast.sol"],]),
        "{r:#}"
    );
}
// --- C# constructors, accessors, indexers and operators (#5054, #5056) -------

fn cs_edges(r: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edges(r)
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| (text(&e["source"]), text(&e["target"])))
        .collect()
}
fn cs_calls(r: &Value) -> BTreeSet<(String, String)> {
    cs_edges(r, "calls")
}
/// `_find`: the id of the node labelled `label` whose id contains `part`.
fn find(r: &Value, label: &str, part: &str) -> String {
    nodes(r)
        .iter()
        .find(|n| n["label"] == label && text(&n["id"]).contains(part))
        .map(|n| text(&n["id"]))
        .unwrap_or_else(|| panic!("{label} ({part}): {r:#}"))
}
/// `_ctor`: the one constructor of `type_name` with `signature`.
fn ctor(r: &Value, type_name: &str, signature: &str) -> String {
    let ids = nodes(r)
        .iter()
        .filter(|n| {
            n["label"] == format!(".{type_name}()").as_str()
                && n["metadata"]["kind"] == "constructor"
                && n["metadata"]["signature"] == signature
        })
        .map(|n| text(&n["id"]))
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 1, "{type_name} {signature}: {r:#}");
    ids[0].clone()
}
fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}
fn cs(files: &[(&str, &str)]) -> Value {
    Fixture::new().extract_files(files)
}

#[test]
fn csharp_construction_links_the_constructor_overload_its_arguments_fit() {
    // test_csharp_object_creation.py
    let r = cs(&[
        (
            "Repo.cs",
            "public class Repo {\n    public Repo() { }\n    public Repo(int a) { }\n    public Repo(int a, int b) { }\n}\n",
        ),
        ("User.cs", "public class User { public void Go() { var r = new Repo(1, 2); } }\n"),
    ]);
    let calls = cs_calls(&r);
    let go = find(&r, ".Go()", "user");
    assert!(calls.contains(&pair(&go, &find(&r, "Repo", "repo"))), "{r:#}");
    assert!(calls.contains(&pair(&go, &ctor(&r, "Repo", "int,int"))), "{r:#}");
    assert!(!calls.contains(&pair(&go, &ctor(&r, "Repo", "int"))));
    assert!(!calls.contains(&pair(&go, &ctor(&r, "Repo", ""))));
}

#[test]
fn csharp_construction_two_overloads_fit_gives_no_constructor_edge() {
    let r = cs(&[(
        "S.cs",
        "public class Repo { public Repo(int a) { } public Repo(string s) { } }\npublic class User { public void Go() { var r = new Repo(1); } }\n",
    )]);
    let calls = cs_calls(&r);
    let go = find(&r, ".Go()", "user");
    assert!(calls.contains(&pair(&go, &find(&r, "Repo", "repo"))), "{r:#}");
    for sig in ["int", "string"] {
        assert!(!calls.contains(&pair(&go, &ctor(&r, "Repo", sig))));
    }
}

#[test]
fn csharp_constructor_body_and_initializer_calls_belong_to_the_constructor() {
    let r = cs(&[(
        "S.cs",
        "public class Base { public Base(int a) { } }\npublic class Svc : Base {\n    public Svc(int x) : base(Seed(x)) { Init(); }\n    static int Seed(int x) { return x; }\n    void Init() { }\n}\n",
    )]);
    let calls = cs_calls(&r);
    let c = ctor(&r, "Svc", "int");
    let svc = find(&r, "Svc", "svc");
    for callee in [".Seed()", ".Init()"] {
        let target = find(&r, callee, "svc");
        assert!(calls.contains(&pair(&c, &target)), "{callee}: {r:#}");
        assert!(!calls.contains(&pair(&svc, &target)), "{callee}");
    }
}

#[test]
fn csharp_this_initializer_links_the_own_overload() {
    let r = cs(&[(
        "S.cs",
        "public class Svc {\n    public Svc() : this(1, 2) { }\n    public Svc(int a) { }\n    public Svc(int a, int b) { }\n}\n",
    )]);
    let calls = cs_calls(&r);
    let empty = ctor(&r, "Svc", "");
    assert!(calls.contains(&pair(&empty, &ctor(&r, "Svc", "int,int"))), "{r:#}");
    assert!(!calls.contains(&pair(&empty, &ctor(&r, "Svc", "int"))));
}

#[test]
fn csharp_base_initializer_links_the_base_constructor_in_another_file() {
    let r = cs(&[
        (
            "Base.cs",
            "namespace Demo {\n    public class Base { public Base(int a) { } public Base(string a, int b) { } }\n}\n",
        ),
        (
            "Derived.cs",
            "namespace Demo {\n    public class Derived : Base { public Derived(int x) : base(\"s\", x) { } }\n}\n",
        ),
    ]);
    let calls = cs_calls(&r);
    let derived = ctor(&r, "Derived", "int");
    assert!(calls.contains(&pair(&derived, &ctor(&r, "Base", "string,int"))), "{r:#}");
    assert!(!calls.contains(&pair(&derived, &ctor(&r, "Base", "int"))));
}

#[test]
fn csharp_base_initializer_with_an_out_of_corpus_base_gives_no_edge() {
    let r = cs(&[(
        "S.cs",
        "public class Local { public Local(int a) { } }\npublic class Derived : System.Exception { public Derived(int x) : base(\"m\") { } }\n",
    )]);
    let derived = ctor(&r, "Derived", "int");
    assert!(cs_calls(&r).iter().all(|(s, _)| *s != derived), "{r:#}");
}

#[test]
fn csharp_static_constructor_is_its_own_node_and_never_constructed() {
    let r = cs(&[(
        "S.cs",
        "public class Reg {\n    static Reg() { Load(); }\n    public Reg() { }\n    static void Load() { }\n}\npublic class User { public void Go() { var r = new Reg(); } }\n",
    )]);
    let calls = cs_calls(&r);
    let (stat, inst) = (ctor(&r, "Reg", "static"), ctor(&r, "Reg", ""));
    assert_ne!(stat, inst);
    assert!(calls.contains(&pair(&stat, &find(&r, ".Load()", "reg"))), "{r:#}");
    let go = find(&r, ".Go()", "user");
    assert!(calls.contains(&pair(&go, &inst)), "{r:#}");
    assert!(!calls.contains(&pair(&go, &stat)));
}

#[test]
fn csharp_primary_constructor_competes_with_the_explicit_ones() {
    let r = cs(&[(
        "S.cs",
        "public record Point(int X, int Y) { public Point(int x) : this(x, 0) { } }\npublic class User { public void Go() { var a = new Point(1, 2); var b = new Point(3); } }\n",
    )]);
    let calls = cs_calls(&r);
    let (primary, explicit) = (ctor(&r, "Point", "int,int"), ctor(&r, "Point", "int"));
    let go = find(&r, ".Go()", "user");
    for p in [pair(&go, &primary), pair(&go, &explicit), pair(&explicit, &primary)] {
        assert!(calls.contains(&p), "{p:?}: {r:#}");
    }
}

#[test]
fn csharp_constructor_never_takes_the_id_of_a_member_spelled_like_its_type() {
    // test_csharp_underscore_collisions.py
    for body in [
        "public class Foo { public Foo() { Bar(); } public void foo() { } public void Bar() { } }\n",
        "public class Foo { public void foo() { } public Foo() { Bar(); } public void Bar() { } }\n",
    ] {
        let r = cs(&[("Foo.cs", body)]);
        let method = format!("{}_foo", find(&r, "Foo", ""));
        assert_eq!(find(&r, ".foo()", ""), method);
        let c = find(&r, ".Foo()", "");
        assert_ne!(c, method);
        let bar = find(&r, ".Bar()", "");
        assert!(cs_calls(&r).contains(&pair(&c, &bar)), "{r:#}");
        assert!(!cs_calls(&r).contains(&pair(&method, &bar)));
    }
}

#[test]
fn csharp_property_accessor_bodies_call_from_the_property() {
    // test_csharp_member_nodes.py
    let r = cs(&[(
        "S.cs",
        "public class Svc {\n    int _v;\n    public int Block { get { return Load(); } set { Save(value); } }\n    public int Arrow { get => Load(); }\n    public int Body => Compute();\n    public int Auto { get; set; } = Seed();\n    int Load() { return _v; }\n    void Save(int v) { _v = v; }\n    int Compute() { return 1; }\n    static int Seed() { return 0; }\n}\n",
    )]);
    let calls = cs_calls(&r);
    let (load, save) = (find(&r, ".Load()", ""), find(&r, ".Save()", ""));
    let block = find(&r, "Block", "");
    for p in [
        pair(&block, &load),
        pair(&block, &save),
        pair(&find(&r, "Arrow", ""), &load),
        pair(&find(&r, "Body", ""), &find(&r, ".Compute()", "")),
    ] {
        assert!(calls.contains(&p), "{p:?}: {r:#}");
    }
    // An initializer runs in the constructor, not in the property.
    assert!(!calls.contains(&pair(&find(&r, "Auto", ""), &find(&r, ".Seed()", ""))));
}

#[test]
fn csharp_accessor_call_prefers_its_own_type_like_a_method() {
    let r = cs(&[(
        "S.cs",
        "public class Svc {\n    public int Value => Load();\n    int Load() { return 1; }\n}\npublic class Other { public int Load() { return 2; } }\n",
    )]);
    let calls = cs_calls(&r);
    let value = find(&r, "Value", "");
    assert!(calls.contains(&pair(&value, &find(&r, ".Load()", "svc"))), "{r:#}");
    assert!(!calls.contains(&pair(&value, &find(&r, ".Load()", "other"))));
}

#[test]
fn csharp_accessor_call_through_a_typed_receiver_resolves_cross_file() {
    let r = cs(&[
        ("Repo.cs", "namespace Demo { public class Repo { public int Count() { return 0; } } }\n"),
        (
            "Svc.cs",
            "namespace Demo { public class Svc {\n    private Repo _repo;\n    public int Total => _repo.Count();\n} }\n",
        ),
    ]);
    assert!(
        cs_calls(&r).contains(&pair(&find(&r, "Total", ""), &find(&r, ".Count()", ""))),
        "{r:#}"
    );
}

#[test]
fn csharp_indexer_is_a_node_and_owns_its_accessor_calls() {
    let r = cs(&[(
        "S.cs",
        "public class Bag {\n    public int This { get; set; }\n    public int this[int i] { get { return Get(i); } }\n    public int this[string k] => Find(k);\n    int Get(int i) { return i; }\n    int Find(string k) { return 0; }\n}\n",
    )]);
    let indexers = nodes(&r)
        .iter()
        .filter(|n| n["metadata"]["kind"] == "indexer")
        .map(|n| (text(&n["metadata"]["signature"]), text(&n["id"])))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(indexers.keys().cloned().collect::<Vec<_>>(), ["int", "string"], "{r:#}");
    assert_ne!(indexers["int"], indexers["string"]);
    // The `This` property keeps the id it had before indexers were nodes.
    let this = find(&r, "This", "");
    assert_eq!(this, format!("{}_this", find(&r, "Bag", "")));
    assert!(!indexers.values().any(|v| *v == this));
    let calls = cs_calls(&r);
    assert!(calls.contains(&pair(&indexers["int"], &find(&r, ".Get()", ""))), "{r:#}");
    assert!(calls.contains(&pair(&indexers["string"], &find(&r, ".Find()", ""))));
}

#[test]
fn csharp_operators_and_conversions_are_nodes_that_own_their_calls() {
    let r = cs(&[(
        "S.cs",
        "public class Money {\n    public static Money operator +(Money a, Money b) => Add(a, b);\n    public static explicit operator int(Money m) { return ToInt(m); }\n    public static explicit operator long(Money m) { return ToLong(m); }\n    static Money Add(Money a, Money b) { return a; }\n    static int ToInt(Money m) { return 0; }\n    static long ToLong(Money m) { return 0; }\n}\n",
    )]);
    let ops = nodes(&r)
        .iter()
        .filter(|n| n["metadata"]["kind"] == "operator")
        .map(|n| (text(&n["label"]), text(&n["id"])))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        ops.keys().cloned().collect::<Vec<_>>(),
        [".operator +()", ".operator int()", ".operator long()"],
        "{r:#}"
    );
    let calls = cs_calls(&r);
    assert!(calls.contains(&pair(&ops[".operator +()"], &find(&r, ".Add()", ""))), "{r:#}");
    assert!(calls.contains(&pair(&ops[".operator int()"], &find(&r, ".ToInt()", ""))));
    assert!(calls.contains(&pair(&ops[".operator long()"], &find(&r, ".ToLong()", ""))));
    assert!(!calls.contains(&pair(&ops[".operator int()"], &find(&r, ".ToLong()", ""))));
}

#[test]
fn csharp_accessor_base_call_resolves_to_the_base_in_another_file() {
    let r = cs(&[
        ("Base.cs", "namespace Demo { public class Base { public int Compute() { return 0; } } }\n"),
        (
            "Svc.cs",
            "namespace Demo { public class Svc : Base {\n    public int Total => base.Compute();\n} }\n",
        ),
    ]);
    assert!(
        cs_calls(&r).contains(&pair(&find(&r, "Total", ""), &find(&r, ".Compute()", ""))),
        "{r:#}"
    );
}

#[test]
fn csharp_accessor_call_binds_the_other_partial_half_over_a_same_named_method() {
    let r = cs(&[
        ("SvcA.cs", "namespace Demo { public partial class Svc { public int Value => Load(); } }\n"),
        ("SvcB.cs", "namespace Demo { public partial class Svc { int Load() { return 1; } } }\n"),
        ("Other.cs", "namespace Demo { public class Other { public int Load() { return 2; } } }\n"),
    ]);
    let calls = cs_calls(&r);
    let value = find(&r, "Value", "");
    assert!(calls.contains(&pair(&value, &find(&r, ".Load()", "svc"))), "{r:#}");
    assert!(!calls.contains(&pair(&value, &find(&r, ".Load()", "other"))));
}

#[test]
fn csharp_explicit_interface_indexers_keep_their_own_calls() {
    let r = cs(&[(
        "S.cs",
        "public interface IA { int this[int i] { get; } }\npublic interface IB { int this[int i] { get; } }\npublic class C : IA, IB {\n    int IA.this[int i] => A();\n    int IB.this[int i] => B();\n    int A() { return 1; }\n    int B() { return 2; }\n}\n",
    )]);
    let by_line = nodes(&r)
        .iter()
        .filter(|n| n["metadata"]["kind"] == "indexer" && text(&n["id"]).contains("_c_"))
        .map(|n| (text(&n["source_location"]), text(&n["id"])))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_line.len(), 2, "{r:#}");
    let calls = cs_calls(&r);
    assert!(calls.contains(&pair(&by_line["L4"], &find(&r, ".A()", ""))), "{r:#}");
    assert!(calls.contains(&pair(&by_line["L5"], &find(&r, ".B()", ""))));
    assert!(!calls.contains(&pair(&by_line["L4"], &find(&r, ".B()", ""))));
}

#[test]
fn csharp_property_sharing_a_methods_id_does_not_lend_it_its_calls() {
    let r = cs(&[(
        "S.cs",
        "public interface IRun { int Next { get; } }\npublic class Run : IRun {\n    int Next() { return Other(); }\n    int IRun.Next => Next();\n    int Other() { return 0; }\n}\n",
    )]);
    let method = find(&r, ".Next()", "");
    let calls = cs_calls(&r);
    assert!(!calls.contains(&pair(&method, &method)), "{r:#}");
    assert!(calls.contains(&pair(&method, &find(&r, ".Other()", ""))));
}

// --- Python: ambiguous import placeholders (#5044) ---------------------------

#[test]
fn ambiguous_import_placeholder_id_is_independent_of_checkout_path() {
    // test_src_layout_import_resolution.py
    let files = [
        ("app/main.py", "from helper import f\n\n\ndef run():\n    return f()\n"),
        ("one/helper.py", "def f():\n    return 1\n"),
        ("two/helper.py", "def f():\n    return 2\n"),
    ];
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let placeholders = |checkout: PathBuf| {
        let paths = files
            .iter()
            .map(|(rel, body)| {
                let path = checkout.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, body).unwrap();
                path
            })
            .collect::<Vec<_>>();
        let r = extract(
            &paths,
            &ExtractOptions {
                root: Some(checkout.clone()),
                cache_root: Some(checkout.join("cache")),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap();
        edges(&r)
            .iter()
            .filter(|e| text(&e["target"]).starts_with("ambiguous_python_import_"))
            .map(|e| (text(&e["source"]), text(&e["target"]), text(&e["source_file"])))
            .collect::<BTreeSet<_>>()
    };
    graphicle::rcfile::set_language_overrides(None);
    let first = placeholders(base.join("checkout-aaa"));
    let second = placeholders(base.join("nested").join("checkout-bbbbbbbb"));
    assert!(!first.is_empty(), "the ambiguous `helper` import keeps a placeholder edge");
    assert_eq!(first, second);
}

// --- YAML: CloudFormation intrinsic tags (#5071, #5077) ----------------------

#[test]
fn cloudformation_intrinsic_tags_do_not_fail_the_whole_file() {
    // test_yaml_extraction.py test_cloudformation_intrinsic_tags_do_not_fail_the_whole_file
    let f = Fixture::new();
    let path = f.write(
        "template.yaml",
        "Conditions:\n  IsProd: !Equals [ !Ref Environment, prod ]\nResources:\n  ExampleFunction:\n    Type: AWS::Serverless::Function\n    Properties:\n      Events:\n        ApiEvent:\n          Properties:\n            Path: /api/example\n",
    );
    let r = graphicle::extractors::yaml_config::extract_yaml(&path);
    assert!(r.get("error").is_none(), "{r:#}");
    let labels = nodes(&r).iter().map(|n| text(&n["label"])).collect::<BTreeSet<_>>();
    for label in ["Conditions", "IsProd", "Resources", "ExampleFunction", "Events", "ApiEvent", "Path"] {
        assert!(labels.contains(label), "{label}: {r:#}");
    }
}

#[test]
fn cloudformation_intrinsic_tag_still_does_not_execute_python_tag() {
    // test_yaml_extraction.py test_cloudformation_intrinsic_tag_still_does_not_execute_python_tag
    let f = Fixture::new();
    let marker = f.root.join("pwned");
    let path = f.write(
        "template.yaml",
        &format!(
            "IsProd: !Equals [ !Ref Environment, prod ]\ndanger: !!python/object/apply:os.system ['touch {}']\n",
            marker.display()
        ),
    );
    let r = graphicle::extractors::yaml_config::extract_yaml(&path);
    assert!(r.get("error").is_some(), "{r:#}");
    assert!(!marker.exists());
}
