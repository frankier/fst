use std::cmp;
use std::collections::BinaryHeap;

use raw::Output;
use Stream;

type BoxedStream<'f> = Box<for<'a> Stream<'a, Item=(&'a [u8], Output)> + 'f>;

#[derive(Copy, Clone, Debug)]
pub struct FstOutput {
    pub index: usize,
    pub output: u64,
}

pub struct StreamOp<'f> {
    streams: Vec<BoxedStream<'f>>,
}

impl<'f> StreamOp<'f> {
    pub fn new() -> Self {
        StreamOp { streams: vec![] }
    }

    pub fn add<S>(mut self, stream: S) -> Self
            where S: 'f + for<'a> Stream<'a, Item=(&'a [u8], Output)> {
        let s = Box::new(stream);
        self.streams.push(s);
        self
    }

    pub fn union(self) -> StreamUnion<'f> {
        StreamUnion {
            heap: StreamHeap::new(self.streams),
            outs: vec![],
            cur_slot: None,
        }
    }

    pub fn intersection(self) -> StreamIntersection<'f> {
        StreamIntersection {
            heap: StreamHeap::new(self.streams),
            outs: vec![],
            cur_slot: None,
        }
    }
}

pub struct StreamUnion<'f> {
    heap: StreamHeap<'f>,
    outs: Vec<FstOutput>,
    cur_slot: Option<Slot>,
}

impl<'a, 'f> Stream<'a> for StreamUnion<'f> {
    type Item = (&'a [u8], &'a [FstOutput]);

    fn next(&'a mut self) -> Option<Self::Item> {
        if let Some(slot) = self.cur_slot.take() {
            self.heap.refill(slot);
        }
        let slot = match self.heap.pop() {
            None => return None,
            Some(slot) => {
                self.cur_slot = Some(slot);
                self.cur_slot.as_ref().unwrap()
            }
        };
        self.outs.clear();
        self.outs.push(slot.fst_output());
        while let Some(slot2) = self.heap.pop_if_equal(slot.input()) {
            self.outs.push(slot2.fst_output());
            self.heap.refill(slot2);
        }
        Some((slot.input(), &self.outs))
    }
}

pub struct StreamIntersection<'f> {
    heap: StreamHeap<'f>,
    outs: Vec<FstOutput>,
    cur_slot: Option<Slot>,
}

impl<'a, 'f> Stream<'a> for StreamIntersection<'f> {
    type Item = (&'a [u8], &'a [FstOutput]);

    fn next(&'a mut self) -> Option<Self::Item> {
        if let Some(slot) = self.cur_slot.take() {
            self.heap.refill(slot);
        }
        loop {
            let slot = match self.heap.pop() {
                None => return None,
                Some(slot) => slot,
            };
            self.outs.clear();
            self.outs.push(slot.fst_output());
            let mut popped: usize = 1;
            while let Some(slot2) = self.heap.pop_if_equal(slot.input()) {
                self.outs.push(slot2.fst_output());
                self.heap.refill(slot2);
                popped += 1;
            }
            if popped < self.heap.num_slots() {
                self.heap.refill(slot);
            } else {
                self.cur_slot = Some(slot);
                let key = self.cur_slot.as_ref().unwrap().input();
                return Some((key, &self.outs))
            }
        }
    }
}

struct StreamHeap<'f> {
    rdrs: Vec<Box<for<'a> Stream<'a, Item=(&'a [u8], Output)> + 'f>>,
    heap: BinaryHeap<Slot>,
}

impl<'f> StreamHeap<'f> {
    fn new(streams: Vec<BoxedStream<'f>>) -> StreamHeap<'f> {
        let mut u = StreamHeap {
            rdrs: streams,
            heap: BinaryHeap::new(),
        };
        for i in 0..u.rdrs.len() {
            u.refill(Slot::new(i));
        }
        u
    }

    fn pop(&mut self) -> Option<Slot> {
        self.heap.pop()
    }

    fn peek_is_duplicate(&self, key: &[u8]) -> bool {
        self.heap.peek().map(|s| s.input() == key).unwrap_or(false)
    }

    fn pop_if_equal(&mut self, key: &[u8]) -> Option<Slot> {
        if self.peek_is_duplicate(key) {
            self.pop()
        } else {
            None
        }
    }

    fn num_slots(&self) -> usize {
        self.rdrs.len()
    }

    fn refill(&mut self, mut slot: Slot) {
        if let Some((input, output)) = self.rdrs[slot.idx].next() {
            slot.set_input(input);
            slot.set_output(output);
            self.heap.push(slot);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Slot {
    idx: usize,
    input: Vec<u8>,
    output: Output,
}

impl Slot {
    fn new(rdr_idx: usize) -> Slot {
        Slot {
            idx: rdr_idx,
            input: Vec::with_capacity(64),
            output: Output::zero(),
        }
    }

    fn fst_output(&self) -> FstOutput {
        FstOutput { index: self.idx, output: self.output.value() }
    }

    fn input(&self) -> &[u8] {
        &self.input
    }

    fn output(&self) -> Output {
        self.output
    }

    fn set_input(&mut self, input: &[u8]) {
        let addcap = input.len().checked_sub(self.input.len()).unwrap_or(0);
        self.input.clear();
        self.input.extend(input);
    }

    fn set_output(&mut self, output: Output) {
        self.output = output;
    }
}

impl PartialOrd for Slot {
    fn partial_cmp(&self, other: &Slot) -> Option<cmp::Ordering> {
        (&self.input, self.output)
        .partial_cmp(&(&other.input, other.output))
        .map(|ord| ord.reverse())
    }
}

impl Ord for Slot {
    fn cmp(&self, other: &Slot) -> cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use raw::build::Builder;
    use raw::tests::{fst_map, fst_set, fst_inputstrs_outputs, fst_input_strs};
    use raw::Fst;
    use {Result, Stream};

    use super::{StreamOp, FstOutput};

    fn s(string: &str) -> String { string.to_owned() }

    fn stream_to_set<I>(mut stream: I) -> Result<Fst>
            where I: for<'a> Stream<'a, Item=(&'a [u8], &'a [FstOutput])> {
        let mut bfst = Builder::memory();
        while let Some((key, _)) = stream.next() {
            try!(bfst.add(key));
        }
        Ok(try!(Fst::from_bytes(try!(bfst.into_inner()))))
    }

    fn stream_to_map<I>(mut stream: I) -> Result<Fst>
            where I: for<'a> Stream<'a, Item=(&'a [u8], &'a [FstOutput])> {
        let mut bfst = Builder::memory();
        while let Some((key, outs)) = stream.next() {
            let merged = outs.iter().fold(0, |a, b| a + b.output);
            try!(bfst.insert(key, merged));
        }
        Ok(try!(Fst::from_bytes(try!(bfst.into_inner()))))
    }

    #[test]
    fn union_set() {
        let set1 = fst_set(&["a", "b", "c"]);
        let set2 = fst_set(&["x", "y", "z"]);

        let op = StreamOp::new()
                              .add(set1.stream()).add(set2.stream())
                              .union();
        let union = stream_to_set(op).unwrap();
        assert_eq!(fst_input_strs(&union), vec!["a", "b", "c", "x", "y", "z"]);
    }

    #[test]
    fn union_set_dupes() {
        let set1 = fst_set(&["aa", "b", "cc"]);
        let set2 = fst_set(&["b", "cc", "z"]);

        let op = StreamOp::new()
                              .add(set1.stream()).add(set2.stream())
                              .union();
        let union = stream_to_set(op).unwrap();
        assert_eq!(fst_input_strs(&union), vec!["aa", "b", "cc", "z"]);
    }

    #[test]
    fn union_map() {
        let map1 = fst_map(vec![("a", 1), ("b", 2), ("c", 3)]);
        let map2 = fst_map(vec![("x", 1), ("y", 2), ("z", 3)]);

        let op = StreamOp::new()
                              .add(map1.stream()).add(map2.stream())
                              .union();
        let union = stream_to_map(op).unwrap();
        assert_eq!(
            fst_inputstrs_outputs(&union),
            vec![
                (s("a"), 1), (s("b"), 2), (s("c"), 3),
                (s("x"), 1), (s("y"), 2), (s("z"), 3),
            ]);
    }

    #[test]
    fn union_map_dupes() {
        let map1 = fst_map(vec![("aa", 1), ("b", 2), ("cc", 3)]);
        let map2 = fst_map(vec![("b", 1), ("cc", 2), ("z", 3)]);
        let map3 = fst_map(vec![("b", 1)]);

        let op = StreamOp::new()
                              .add(map1.stream())
                              .add(map2.stream())
                              .add(map3.stream())
                              .union();
        let union = stream_to_map(op).unwrap();
        assert_eq!(
            fst_inputstrs_outputs(&union),
            vec![
                (s("aa"), 1), (s("b"), 4), (s("cc"), 5), (s("z"), 3),
            ]);
    }

    #[test]
    fn intersect_set() {
        let sets = &[
            fst_set(&["a", "b", "c"]),
            fst_set(&["x", "y", "z"]),
        ];
        let op = StreamOp::new()
                              .add(sets[0].stream()).add(sets[1].stream())
                              .intersection();
        let inter_stream = stream_to_set(op).unwrap();
        assert_eq!(fst_input_strs(&inter_stream), Vec::<&str>::new());
    }

    #[test]
    fn intersect_set_dupes() {
        let sets = &[
            fst_set(&["aa", "b", "cc"]),
            fst_set(&["b", "cc", "z"]),
        ];
        let op = StreamOp::new()
                              .add(sets[0].stream()).add(sets[1].stream())
                              .intersection();
        let inter_stream = stream_to_set(op).unwrap();
        assert_eq!(fst_input_strs(&inter_stream), vec!["b", "cc"]);
    }

    #[test]
    fn intersect_map() {
        let maps = &[
            fst_map(vec![("a", 1), ("b", 2), ("c", 3)]),
            fst_map(vec![("x", 1), ("y", 2), ("z", 3)]),
        ];
        let op = StreamOp::new()
                              .add(maps[0].stream()).add(maps[1].stream())
                              .intersection();
        let inter_stream = stream_to_map(op).unwrap();
        assert_eq!(fst_inputstrs_outputs(&inter_stream),
                   Vec::<(String, u64)>::new());
    }

    #[test]
    fn intersect_map_dupes() {
        let maps = &[
            fst_map(vec![("aa", 1), ("b", 2), ("cc", 3)]),
            fst_map(vec![("b", 1), ("cc", 2), ("z", 3)]),
            fst_map(vec![("b", 1)]),
        ];
        let op = StreamOp::new()
                              .add(maps[0].stream())
                              .add(maps[1].stream())
                              .add(maps[2].stream())
                              .intersection();
        let inter_stream = stream_to_map(op).unwrap();
        assert_eq!(fst_inputstrs_outputs(&inter_stream), vec![(s("b"), 4)]);
    }
}
