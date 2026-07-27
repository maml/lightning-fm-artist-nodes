// Dev helper — print a fresh BIP39 mnemonic for test-node identities.
// (Public test-vector mnemonics get swept by bots on shared networks.)

fn main() {
    println!("{}", ldk_node::generate_entropy_mnemonic(None));
}
