use crate::{
    binary::{
        leaf_sum,
        node_sum,
    },
    common::{
        Bytes32,
        ProofSet,
    },
};

pub fn verify<T: AsRef<[u8]>>(
    root: &Bytes32,
    data: &T,
    proof_set: &ProofSet,
    proof_index: u64,
    num_leaves: u64,
) -> Option<bool> {
    let mut sum = leaf_sum(data.as_ref());

    if proof_index >= num_leaves {
        return false.into()
    }

    if proof_set.is_empty() {
        return (if num_leaves == 1 { *root == sum } else { false }).into()
    }

    let mut height = 1usize;
    let mut stable_end = proof_index;

    loop {
        let subtree_start_index = proof_index
            .checked_div(1 << height)
            .and_then(|x| x.checked_mul(1 << height))?;
        let subtree_end_index = subtree_start_index
            .checked_add(1 << height)
            .and_then(|x| x.checked_sub(1))?;

        if subtree_end_index >= num_leaves {
            break
        }

        stable_end = subtree_end_index;

        if proof_set.len() < height {
            return Some(false)
        }

        let height_index = height.checked_sub(1)?;
        let proof_data = proof_set[height_index];
        let index_difference = proof_index.checked_sub(subtree_start_index)?;
        if index_difference < 1 << height_index {
            sum = node_sum(&sum, &proof_data);
        } else {
            sum = node_sum(&proof_data, &sum);
        }

        height = height.checked_add(1)?;
    }

    let leaf_index = num_leaves
        .checked_sub(1)
        .expect("Program should panic if this overflows");
    if stable_end != leaf_index {
        if proof_set.len() < height {
            return Some(false)
        }
        let height_index = height.checked_sub(1)?;
        let proof_data = proof_set[height_index];
        sum = node_sum(&sum, &proof_data);
        height = height.checked_add(1)?;
    }

    while height.checked_sub(1)? < proof_set.len() {
        let height_index = height.checked_sub(1)?;
        let proof_data = proof_set[height_index];
        sum = node_sum(&proof_data, &sum);
        height = height.checked_add(1)?;
    }

    Some(sum == *root)
}

#[cfg(test)]
mod test {
    use super::verify;
    use crate::{
        binary::{
            MerkleTree,
            Primitive,
        },
        common::StorageMap,
    };
    use fuel_merkle_test_helpers::TEST_DATA;
    use fuel_storage::Mappable;

    #[derive(Debug)]
    struct TestTable;

    impl Mappable for TestTable {
        type Key = Self::OwnedKey;
        type OwnedKey = u64;
        type OwnedValue = Primitive;
        type Value = Self::OwnedValue;
    }

    #[test]
    fn verify_returns_true_when_the_given_proof_set_matches_the_given_merkle_root() {
        let mut storage_map = StorageMap::<TestTable>::new();
        let mut tree = MerkleTree::new(&mut storage_map);

        const PROOF_INDEX: usize = 2;
        const LEAVES_COUNT: usize = 5;

        let data = &TEST_DATA[0..LEAVES_COUNT]; // 5 leaves
        for datum in data.iter() {
            tree.push(datum).unwrap();
        }

        let (root, proof_set) = tree.prove(PROOF_INDEX as u64).unwrap();
        let verification = verify(
            &root,
            &TEST_DATA[PROOF_INDEX],
            &proof_set,
            PROOF_INDEX as u64,
            LEAVES_COUNT as u64,
        )
        .unwrap();
        assert!(verification);
    }

    #[test]
    fn verify_returns_false_when_the_given_proof_set_does_not_match_the_given_merkle_root(
    ) {
        // Check the Merkle root of one tree against the computed Merkle root of
        // another tree's proof set: because the two roots come from different
        // trees, the comparison should fail.

        // Generate the first Merkle tree and get its root
        let mut storage_map = StorageMap::<TestTable>::new();
        let mut tree = MerkleTree::new(&mut storage_map);

        const PROOF_INDEX: usize = 2;
        const LEAVES_COUNT: usize = 5;

        let data = &TEST_DATA[0..LEAVES_COUNT - 1];
        for datum in data.iter() {
            tree.push(datum).unwrap();
        }
        let proof = tree.prove(PROOF_INDEX as u64).unwrap();
        let root = proof.0;

        // Generate the second Merkle tree and get its proof set
        let mut storage_map = StorageMap::<TestTable>::new();
        let mut tree = MerkleTree::new(&mut storage_map);

        let data = &TEST_DATA[5..10];
        for datum in data.iter() {
            tree.push(datum).unwrap();
        }
        let proof = tree.prove(PROOF_INDEX as u64).unwrap();
        let set = proof.1;

        let verification = verify(
            &root,
            &TEST_DATA[PROOF_INDEX],
            &set,
            PROOF_INDEX as u64,
            LEAVES_COUNT as u64,
        )
        .unwrap();
        assert!(!verification);
    }

    #[test]
    fn verify_returns_false_when_the_proof_set_is_empty() {
        const PROOF_INDEX: usize = 0;
        const LEAVES_COUNT: usize = 0;

        let verification = verify(
            &Default::default(),
            &TEST_DATA[PROOF_INDEX],
            &vec![],
            PROOF_INDEX as u64,
            LEAVES_COUNT as u64,
        )
        .unwrap();
        assert!(!verification);
    }

    #[test]
    fn verify_returns_false_when_the_proof_index_is_invalid() {
        let mut storage_map = StorageMap::<TestTable>::new();
        let mut tree = MerkleTree::new(&mut storage_map);

        const PROOF_INDEX: usize = 0;
        const LEAVES_COUNT: usize = 5;

        let data = &TEST_DATA[0..LEAVES_COUNT - 1];
        for datum in data.iter() {
            tree.push(datum).unwrap();
        }

        let proof = tree.prove(PROOF_INDEX as u64).unwrap();
        let root = proof.0;
        let set = proof.1;

        let verification = verify(
            &root,
            &TEST_DATA[PROOF_INDEX],
            &set,
            PROOF_INDEX as u64 + 15,
            LEAVES_COUNT as u64,
        )
        .unwrap();
        assert!(!verification);
    }
}
