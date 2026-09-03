use super::*;
use crate::pristine::txn::helpers::collect_until_error;

// TreeTxnT Implementation

impl<'a> TreeTxnT for WriteTxn<'a> {
    fn get_inode(&self, path: &str) -> PristineResult<Option<Inode>> {
        let table = self.txn.open_table(TREE)?;
        let result = table.get(path)?;
        match result {
            Some(value) => Ok(Some(Inode::new(value.value()))),
            None => Ok(None),
        }
    }

    fn get_directory_flags(&self, inode: Inode) -> PristineResult<Option<u8>> {
        let table = self.txn.open_table(DIRECTORIES)?;
        let result = table.get(inode.get())?;
        Ok(result.map(|v| v.value()))
    }

    fn get_path(&self, inode: Inode) -> PristineResult<Option<String>> {
        let table = self.txn.open_table(REV_TREE)?;
        let result = table.get(inode.get())?;
        match result {
            Some(value) => Ok(Some(value.value().to_string())),
            None => Ok(None),
        }
    }

    fn inode_position(&self, inode: Inode) -> PristineResult<Option<Position<NodeId>>> {
        let table = self.txn.open_table(INODES)?;
        let result = table.get(inode.get())?;
        match result {
            Some(value) => {
                let (change_id, pos) = decode_position(value.value());
                Ok(Some(Position::new(
                    NodeId::new(change_id),
                    ChangePosition::new(pos),
                )))
            }
            None => Ok(None),
        }
    }

    fn position_inode(&self, pos: Position<NodeId>) -> PristineResult<Option<Inode>> {
        let table = self.txn.open_table(REV_INODES)?;
        let key = encode_position(pos.change.get(), pos.pos.get());
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(Inode::new(value.value()))),
            None => Ok(None),
        }
    }

    fn iter_tree(
        &self,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>> {
        let table = self.txn.open_table(TREE)?;
        let results = collect_until_error(table.iter()?.map(|result| {
            result
                .map(|(key, value)| (key.value().to_string(), Inode::new(value.value())))
                .map_err(|error| PristineError::Storage(Box::new(error)))
        }));
        Ok(Box::new(results.into_iter()))
    }

    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> PristineResult<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
    > {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);

        let mut results = Vec::new();
        'entries: for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, values) = match result {
                Ok(entry) => entry,
                Err(error) => {
                    results.push(Err(PristineError::Storage(Box::new(error))));
                    break;
                }
            };
            let (_, change_id, start, end) = decode_inode_vertex(key.value());
            let node = GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(start),
                end: ChangePosition::new(end),
            };

            for value in values {
                match value {
                    Ok(value) => {
                        let edge = deserialize_edge(value.value());
                        results.push(Ok((node, edge)));
                    }
                    Err(error) => {
                        results.push(Err(PristineError::Storage(Box::new(error))));
                        break 'entries;
                    }
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn get_file_index(&self, path: &str) -> PristineResult<Option<FileIndexMetadata>> {
        let table = self.txn.open_table(FILE_INDEX)?;
        let guard = table.get(path)?;
        match guard {
            Some(value) => {
                let bytes = value.value();
                let (secs, nanos, size, hash) = decode_file_index(bytes);
                Ok(Some((secs, nanos, size, hash)))
            }
            None => Ok(None),
        }
    }

    fn iter_file_index(&self) -> PristineResult<Vec<FileIndexEntry>> {
        let table = match self.txn.open_table(FILE_INDEX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };
        let mut entries = Vec::new();
        for result in table.iter()? {
            let (key, value) = result?;
            let path = key.value().to_string();
            let (secs, nanos, size, hash) = decode_file_index(value.value());
            entries.push((path, secs, nanos, size, hash));
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{MutTxnT, Pristine};
    use tempfile::tempdir;

    #[test]
    fn healthy_write_tree_and_inode_iterators_return_all_values() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let inode = Inode::new(11);
        txn.put_tree("src/lib.rs", inode).unwrap();

        let change_id = txn
            .register_change(&Hash::of(b"write inode graph"))
            .unwrap();
        let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(4));
        for position in [2, 1] {
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(
                    EdgeFlags::BLOCK,
                    Position::new(change_id, ChangePosition::new(position)),
                    change_id,
                ),
            )
            .unwrap();
        }

        let tree = txn
            .iter_tree()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(tree, vec![("src/lib.rs".to_string(), inode)]);

        let inode_entries = txn
            .iter_inode_vertices(inode)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            inode_entries
                .iter()
                .map(|(_, edge)| edge.dest().pos.get())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
