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
                vec![text(&s["label"]), text(&t["label"]), file_name(&t["source_file"])]
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
    assert_eq!(id("grantRole()"), format!("{contract}_function_grantrole_2_bytes32_address"));
    assert_eq!(id("hasRole()"), format!("{contract}_function_hasrole_2_bytes32_address"));
    assert_eq!(id("RoleGranted"), format!("{contract}_event_rolegranted_rolegranted"));
    assert!(id("_grantRole()").starts_with(&format!("{contract}_function_grantrole_2_bytes32_address_")));
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
    assert_eq!(node(&r, &target).unwrap()["metadata"]["first_param_type"], "uint256");
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
        strs(&[
            &["pay()", "transfer()", "IERC20.sol"],
        ]),
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
        strs(&[
            &["touch()", "toUint48()", "SafeCast.sol"],
        ]),
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
        strs(&[
            &["ok()", "g()", "Over.sol"],
        ]),
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
        strs(&[
            &["ok()", "toUint48()", "SafeCast.sol"],
        ]),
        "{r:#}"
    );
}

