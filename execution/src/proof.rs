use ethers::types::{Bytes, EIP1186ProofResponse};
use ethers::utils::keccak256;
use ethers::utils::rlp::{decode_list, RlpStream};

// responsible for checking whether a given proof is valid by verifying that the proof can either confirm the existence (inclusion proof) or non-existence (exclusion proof) of a key-value pair in a Merkle Patricia Trie.
//Proof: array of bytes representing the proof 
//Root: root hash of the trie 
//PAth: Key that we want to verify 
//Value: the value expected to be associated with path in the trie 
pub fn verify_proof(proof: &[Bytes], root: &[u8], path: &[u8], value: &[u8]) -> bool {
    let mut expected_hash = root.to_vec();
    let mut path_offset = 0;//keeps track of the current nibble in the path

    for (i, node) in proof.iter().enumerate() {//iterates through each node in the proof array 
        //root is the first node in the proof
        if expected_hash != keccak256(node).to_vec() { //checks if the hash of the node is equal to the expected hash
            return false;
        }

        let node_list: Vec<Vec<u8>> = decode_list(node);//decodes the node contents into a list of vectors
        //node has 17 entries corresponding to the possible nibbles (0-15) plus a value entry.(valid for branch nodes )

        if node_list.len() == 17 {
            //node with 17 entries is branch node 
            // The first 16 entries correspond to possible nibbles (0-15), and the 17th entry is a value if the node represents a key with a value at that specific point in the trie.ie a value entry 
            //exclusion proof: demonstrates that key value pair does not exist in the trie
            //inclusion prroof : demonstrates that key value pair exists in the trie 
            if i == proof.len() - 1 {//index of last node in the proof of array : if equal to i  i.e index of current node in the proof array
                // exclusion proof: at the end of proof
                let nibble = get_nibble(path, path_offset);//gets the current nibble from the path
                let node = &node_list[nibble as usize];
                //The function extracts the current nibble (a 4-bit segment) from the path based on the path_offset and uses this nibble to index into the node_list

                if node.is_empty() && is_empty_value(value) {
                    return true;//If the node at the current nibble is empty and the value is empty, it indicates that the key is not present, thus proving its exclusion.
                }

                //if node is not empty we check for inclusion 
            } else {
                let nibble = get_nibble(path, path_offset);
                expected_hash.clone_from(&node_list[nibble as usize]);//updated expected hash with the hash of the node at the current nibble

                path_offset += 1;
            }
        } else if node_list.len() == 2 {//node with 2 entries is extension node 
            //Node Path (node_list[0]): 
            //Value or Hash (node_list[1]):either the value associated with the key or a hash pointing to another node further down the trie.

            if i == proof.len() - 1 {
                // exclusion proof
                //If the path in the node doesn't match the expected path, and the value is empty, this proves that the key does not exist.
                if !paths_match(&node_list[0], skip_length(&node_list[0]), path, path_offset)
                    && is_empty_value(value)
                {
                    return true;
                }

                // inclusion proof
                if node_list[1] == value {
                    return paths_match(
                        &node_list[0],
                        skip_length(&node_list[0]),
                        path,
                        path_offset,
                    );
                }//If the value matches and the paths match, this proves that the key exists in the trie.

            } else {
                let node_path = &node_list[0];
                let prefix_length = shared_prefix_length(path, path_offset, node_path);//The function checks how much of the path matches the node path. If they diverge too soon, the proof is invalid.
                if prefix_length < node_path.len() * 2 - skip_length(node_path) {
                    // The proof shows a divergent path, but we're not at the end of the proof, so something's wrong.
                    return false;
                }
                path_offset += prefix_length;
                expected_hash.clone_from(&node_list[1]);
            }
        } else {
            return false;
        }
    }

    false
}

fn paths_match(p1: &[u8], s1: usize, p2: &[u8], s2: usize) -> bool {//p1 , p2 denote path ; s1,s2 denote offset in the path
    //checks whether the remaining portion of the path starting from the specified offset is the same for both paths
    //each byte in the path is represented by two nibbles 
    //nibble consistes of 4 bits and byte consists of 8 bits 
    /*
    A byte represented by the hexadecimal value 0xAB can be split into:
High nibble: 0xA (binary: 1010)
Low nibble: 0xB (binary: 1011) */

// The offset indicates the position within this sequence where a certain operation or comparison should start.
/*If you have an offset of 0, you're looking at the very beginning of the path.
If you have an offset of 1, you're looking at the second nibble of the path (which could be the low nibble of the first byte or the high nibble of the second byte,
 depending on the position). */
 /*
 A path is stored as a sequence of bytes, but logically it is processed as a sequence of nibbles.
For example, the path [0xAB, 0xCD] would be treated as the nibble sequence [A, B, C, D]
 */
/*offset / 2: This calculates which byte in the array corresponds to the nibble we're interested in. For example:

If offset is 0 or 1, this corresponds to the first byte (path[0]).
If offset is 2 or 3, this corresponds to the second byte (path[1]).*/
    let len1 = p1.len() * 2 - s1; 
    let len2 = p2.len() * 2 - s2;

    if len1 != len2 {//checks legth if unequal then false 
        return false;
    }

    for offset in 0..len1 {
        let n1 = get_nibble(p1, s1 + offset);
        let n2 = get_nibble(p2, s2 + offset);

        if n1 != n2 {
            return false;
        }
    }

    true
}

#[allow(dead_code)]
fn get_rest_path(p: &[u8], s: usize) -> String {//path is represented as slice of bytes 
    let mut ret = String::new();
    for i in s..p.len() * 2 { //p.len*2 gives byte length in nibbles
        let n = get_nibble(p, i);
        //The get_nibble function isolates either the high or low nibble of the appropriate byte, depending on whether i is even or odd.
        //0: high 1: low and so on 
        ret += &format!("{n:01x}");//n is converted to hex string of one character 
    }
    ret
}
//checks whether ta given value in trie represents an empty value either as empyt stroreage slot or empty account
fn is_empty_value(value: &[u8]) -> bool {
    //RLP (Recursive Length Prefix) encoding is a method used to serialize complex data structures into a simple byte array.
    //stream is used to serialize the data
    let mut stream = RlpStream::new();
    stream.begin_list(4);//appends 4 empty data to the stream
    //which are account informatino 
    //nonce, balance, storage root, and code hash.

    stream.append_empty_data();
    stream.append_empty_data();
    //empty nonce 
    //emppty balance
    let empty_storage_hash = "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421";// This hash is a known constant in Ethereum that represents an empty storage root.
    // It is the Keccak-256 hash of an empty string.
    stream.append(&hex::decode(empty_storage_hash).unwrap());
    let empty_code_hash = "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";//known has representing an empty account code 
    stream.append(&hex::decode(empty_code_hash).unwrap());
    let empty_account = stream.out();
// finalizes the RLP encoding of the empty account by outputting the serialized data from the RlpStream.

//checks if value is empty slot In RLP encoding, an empty string or a value of zero is represented as 0x80, which is a single byte.
    let is_empty_slot = value.len() == 1 && value[0] == 0x80;
    //checks if value is empty account
    let is_empty_account = value == empty_account; //indicates account is empty i.e with no nonce , balance , storage or code
    is_empty_slot || is_empty_account
}
// calculates the length of the common prefix between two paths in a Merkle Patricia Trie, 

fn shared_prefix_length(path: &[u8], path_offset: usize, node_path: &[u8]) -> usize {
    //we are comparing path against the node path 
    //path_offset is the offset in the path from where we start comparing with node path 
    let skip_length = skip_length(node_path);//The skip_length function determines how many nibbles should be skipped for the given node_path

    let len = std::cmp::min(// ensures that you don't attempt to compare more nibbles than are available in either the node_path or path.
        node_path.len() * 2 - skip_length,//no of nibbles in node path after skipping
        path.len() * 2 - path_offset,//no of nibbles in path after offset
    );
    let mut prefix_len = 0;

    //The loop compares corresponding nibbles from path and node_path.
    for i in 0..len {
        let path_nibble = get_nibble(path, i + path_offset);
        let node_path_nibble = get_nibble(node_path, i + skip_length);

        if path_nibble == node_path_nibble {
            prefix_len += 1;
        } else {
            break;
        }
    }

    prefix_len
}
fn skip_length(node: &[u8]) -> usize {
    if node.is_empty() {
        return 0;
    }//no need to skip if node is empty

    let nibble = get_nibble(node, 0);
    match nibble {
        0 => 2, // If the first nibble is 0, we skip 2 nibbles ; 0 represents branch node 
        //he node's path represents a full 2-nibble (1-byte) key, and it might be necessary to skip both nibbles to correctly align with the next path segment in the trie
        1 => 1,
        //Here, the skip length of 1 indicates that only the first nibble needs to be skipped, as the node's path involves only one byte of data.
        2 => 2,
        //This value is similar to 0, indicating a node with a 2-nibble (1-byte) path. The difference is that this value can be used for nodes containing more complex data structures or multiple bytes.
        3 => 1,
        //This value represents a short path where the first nibble is the only relevant part of the path.
        _ => 0,
    }
}

fn get_nibble(path: &[u8], offset: usize) -> u8 {
    let byte = path[offset / 2];//since offset is specified in nibbles so we have to half the amount of nibbles to get bytes  
    if offset % 2 == 0 {//high nibble 
        byte >> 4//discards the lower 4 bits and we have upper 4 bits remaining 
    } else {
        byte & 0xF //low nibble extracteed 
        //0xF is 00001111 and byte in binary is comparted with it so that we extract only the lower 4 digits 
    }
}
// encode the account data from an EIP1186ProofResponse into a format suitable for use in Ethereum, specifically using RLP (Recursive Length Prefix) encodi
pub fn encode_account(proof: &EIP1186ProofResponse) -> Vec<u8> {
    let mut stream = RlpStream::new_list(4);
    stream.append(&proof.nonce);
    stream.append(&proof.balance);
    stream.append(&proof.storage_hash);
    stream.append(&proof.code_hash);
    let encoded = stream.out();//stream.out() finalizes the encoding and returns the RLP-encoded data as a Bytes type.

    encoded.to_vec()
}

#[cfg(test)]
mod tests {
    use crate::proof::shared_prefix_length;

    #[tokio::test]
    async fn test_shared_prefix_length() {
        // We compare the path starting from the 6th nibble i.e. the 6 in 0x6f
        let path: Vec<u8> = vec![0x12, 0x13, 0x14, 0x6f, 0x6c, 0x64, 0x21];
        let path_offset = 6;
        // Our node path matches only the first 5 nibbles of the path
        let node_path: Vec<u8> = vec![0x6f, 0x6c, 0x63, 0x21];
        let shared_len = shared_prefix_length(&path, path_offset, &node_path);
        assert_eq!(shared_len, 5);

        // Now we compare the path starting from the 5th nibble i.e. the 4 in 0x14
        let path: Vec<u8> = vec![0x12, 0x13, 0x14, 0x6f, 0x6c, 0x64, 0x21];
        let path_offset = 5;
        // Our node path matches only the first 7 nibbles of the path
        // Note the first nibble is 1, so we skip 1 nibble
        let node_path: Vec<u8> = vec![0x14, 0x6f, 0x6c, 0x64, 0x11];
        let shared_len = shared_prefix_length(&path, path_offset, &node_path);
        assert_eq!(shared_len, 7);
    }
}
