extern crate fnv;
#[cfg(feature = "print_cache_stats")]
use std::cell::UnsafeCell;
use std::cmp;
use std::marker::PhantomData;
use std::ptr;

use super::art_internal::MarkedPtr;

pub use self::dense_hash_set::HashSetPrefixCache;

/// PrefixCache describes types that can cache pointers interior to an ART.
pub trait PrefixCache<T> {
    /// If true, the cache is used during ART set operations. If false, the cache is ignored.
    const ENABLED: bool;
    /// If true, lookup returning None indicates that no nodes with prefix `bs` are in the set.
    const COMPLETE: bool;
    fn new() -> Self;
    fn lookup(&self, bs: &[u8]) -> Option<MarkedPtr<T>>;
    fn replace(&mut self, bs: &[u8], ptr: MarkedPtr<T>) -> Option<MarkedPtr<T>> {
        self.insert(bs, ptr);
        None
    }
    fn insert(&mut self, bs: &[u8], ptr: MarkedPtr<T>) {
        let _ = self.replace(bs, ptr);
    }
    #[inline(always)]
    fn debug_assert_unreachable(&self, _ptr: MarkedPtr<T>) {}
}
pub struct NullBuckets<T>(PhantomData<T>);

impl<T> PrefixCache<T> for NullBuckets<T> {
    const ENABLED: bool = false;
    const COMPLETE: bool = false;
    fn new() -> Self {
        NullBuckets(PhantomData)
    }
    fn lookup(&self, _: &[u8]) -> Option<MarkedPtr<T>> {
        None
    }
    fn insert(&mut self, _: &[u8], _ptr: MarkedPtr<T>) {}
}

mod dense_hash_set {
    use super::*;
    use super::fnv::FnvHasher;
    use super::super::Digital;
    use super::super::byteorder::{BigEndian, ByteOrder};

    use std::hash::{Hash, Hasher};
    use std::mem;

    fn read_u64(bs: &[u8]) -> u64 {
        debug_assert!(bs.len() <= 8);
        let mut arr = [0 as u8; 8];
        unsafe { ptr::copy_nonoverlapping(&bs[0], &mut arr[0], cmp::min(bs.len(), 8)) };
        BigEndian::read_u64(&arr[..])
    }

    pub struct HashSetPrefixCache<T>(DenseHashTable<MarkedElt<T>>);
    impl<T> PrefixCache<T> for HashSetPrefixCache<T> {
        const ENABLED: bool = true;
        const COMPLETE: bool = true;
        fn new() -> Self {
            HashSetPrefixCache(DenseHashTable::new())
        }

        #[cfg(debug_assertions)]
        fn debug_assert_unreachable(&self, ptr: MarkedPtr<T>) {
            for elt in self.0.buckets.iter() {
                if elt.ptr == ptr {
                    assert!(
                        self.0.lookup(&elt.prefix).is_some(),
                        "attempted to look up {:?}:{:?} but failed",
                        elt.prefix,
                        elt.ptr
                    );
                    let l = self.0.lookup(&elt.prefix).unwrap();
                    assert!(l.ptr == elt.ptr, "got {:?} != elt {:?}", l, elt);
                    assert!(
                        elt.ptr != ptr,
                        "Found ptr {:?} in elt with prefix {:?} [{:?}]",
                        ptr,
                        elt.prefix,
                        elt.prefix.digits().collect::<Vec<u8>>().as_slice()
                    )
                }
            }
        }

        fn lookup(&self, bs: &[u8]) -> Option<MarkedPtr<T>> {
            let prefix = read_u64(bs);
            let res = self.0.lookup(&prefix).map(|elt| elt.ptr.clone());
            #[cfg(debug_assertions)]
            unsafe {
                if let Some(Err(inner)) = res.as_ref()
                    .map(|x| x.get().expect("stored pointer should be non-null"))
                {
                    assert!(
                        inner.children != !0,
                        "Returning an expired node {:?} (ty={:?})",
                        res,
                        inner.typ
                    );
                }
            }
            res
        }

        fn insert(&mut self, bs: &[u8], ptr: MarkedPtr<T>) {
            let prefix = read_u64(bs);
            if ptr.is_null() {
                self.0.delete(&prefix);
                debug_assert!(self.lookup(bs).is_none());
            } else {
                let _ = self.0.insert(MarkedElt {
                    prefix: prefix,
                    ptr: ptr,
                });
            }
        }

        fn replace(&mut self, bs: &[u8], ptr: MarkedPtr<T>) -> Option<MarkedPtr<T>> {
            let prefix = read_u64(bs);
            if ptr.is_null() {
                self.0.delete(&prefix)
            } else {
                match self.0.insert(MarkedElt {
                    prefix: prefix,
                    ptr: ptr,
                }) {
                    Ok(()) => None,
                    Err(t) => Some(t),
                }
            }.map(|t| t.ptr)
        }
    }

    trait DHTE {
        type Key;
        fn null() -> Self;
        fn tombstone() -> Self;
        fn is_null(&self) -> bool;
        fn is_tombstone(&self) -> bool;
        fn key(&self) -> &Self::Key;
    }

    const MARKED_TOMBSTONE: usize = !0;
    struct MarkedElt<T> {
        prefix: u64,
        ptr: MarkedPtr<T>,
    }
    impl<T> ::std::fmt::Debug for MarkedElt<T> {
        fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
            write!(
                f,
                "MarkedElt{{ {:?}, {:?} }}",
                self.prefix.digits().collect::<Vec<u8>>().as_slice(),
                self.ptr
            )
        }
    }

    impl<T> DHTE for MarkedElt<T> {
        type Key = u64;
        fn null() -> Self {
            MarkedElt {
                prefix: 0,
                ptr: MarkedPtr::null(),
            }
        }
        fn tombstone() -> Self {
            MarkedElt {
                prefix: 0,
                ptr: MarkedPtr::from_leaf(MARKED_TOMBSTONE as *mut T),
            }
        }

        fn is_null(&self) -> bool {
            self.ptr.is_null()
        }
        fn is_tombstone(&self) -> bool {
            self.ptr.raw_eq(MARKED_TOMBSTONE)
        }
        fn key(&self) -> &Self::Key {
            &self.prefix
        }
    }

    /// A bare-bones implementation of Google's dense_hash_set. Not a full-featured map, but
    /// contains sufficient functionality to be used as a PrefixCache
    ///
    /// TODO: explore optimizing this more (for time or for space).
    struct DenseHashTable<T> {
        buckets: Vec<T>,
        len: usize,
        set: usize,
    }

    impl<T: DHTE> DenseHashTable<T>
    where
        T::Key: Eq + Hash,
    {
        fn next_probe(hash: usize, i: usize) -> usize {
            // hash + i
            hash + (i + i * i) / 2
        }

        fn new() -> Self {
            DenseHashTable {
                buckets: Vec::new(),
                len: 0,
                set: 0,
            }
        }

        fn seek(
            &self,
            k: &T::Key,
        ) -> (
            Option<*mut T>, /* first tombstone */
            Option<*mut T>, /* matching or null */
        ) {
            let mut tombstone = None;
            let l = self.buckets.len();
            debug_assert!(l.is_power_of_two());
            let hash = {
                let mut hasher = FnvHasher::default();
                k.hash(&mut hasher);
                hasher.finish() as usize
            };
            let mut ix = hash;
            let mut times = 0;
            while times < l {
                ix &= l - 1;
                debug_assert!(ix < self.buckets.len());
                times += 1;
                let bucket = unsafe { self.buckets.get_unchecked(ix) };
                let bucket_raw = bucket as *const T as *mut T;
                if tombstone.is_none() && bucket.is_tombstone() {
                    tombstone = Some(bucket_raw);
                } else if bucket.is_null() || bucket.key() == k {
                    return (tombstone, Some(bucket_raw));
                }
                ix = Self::next_probe(hash, times);
            }
            (tombstone, None)
        }

        fn grow(&mut self) {
            debug_assert!(self.set >= self.len);
            let old_len = if self.buckets.len() == 0 {
                self.buckets.push(T::null());
                return;
            } else if self.buckets.len() < 32
                || (self.set as i64) - (self.len as i64) < (self.buckets.len() as i64 / 4)
            {
                // actually grow. If this condition is not met, then we just re-hash
                let l = self.buckets.len();
                self.buckets.extend((0..l).map(|_| T::null()));
                l
            } else {
                self.buckets.len()
            };
            debug_assert!(self.buckets.len().is_power_of_two());
            debug_assert!(old_len.is_power_of_two());
            let mut v = Vec::with_capacity(self.len);
            for i in &mut self.buckets[0..old_len] {
                if i.is_null() {
                    continue;
                }
                if i.is_tombstone() {
                    *i = T::null();
                    continue;
                }
                let mut t = T::null();
                mem::swap(i, &mut t);
                v.push(t);
            }
            self.set = 0;
            self.len = 0;
            for elt in v.into_iter() {
                let _res = self.insert(elt);
                debug_assert!(_res.is_ok());
            }
        }

        fn lookup(&self, k: &T::Key) -> Option<&T> {
            if self.buckets.len() == 0 {
                return None;
            }
            let (_, b_opt) = self.seek(k);
            b_opt.and_then(|b| unsafe {
                if (*b).is_null() {
                    None
                } else {
                    Some(&*b)
                }
            })
        }

        fn delete(&mut self, k: &T::Key) -> Option<T> {
            if self.buckets.len() == 0 {
                return None;
            }
            let (_, b_opt) = self.seek(k);
            b_opt.and_then(|b| unsafe {
                if (*b).is_null() {
                    None
                } else {
                    let mut tomb = T::tombstone();
                    mem::swap(&mut *b, &mut tomb);
                    self.len -= 1;
                    Some(tomb)
                }
            })
        }

        fn insert(&mut self, mut t: T) -> Result<(), T> {
            if self.set >= self.buckets.len() / 2 {
                self.grow();
            }
            debug_assert!(!t.is_null());
            debug_assert!(!t.is_tombstone());
            let (tmb, b_opt) = self.seek(t.key());
            unsafe {
                let bucket = b_opt.unwrap();
                if (*bucket).is_null() {
                    // t is not already in the table. We insert it somewhere
                    if let Some(tombstone_bucket) = tmb {
                        // there was a tombstone earlier in the probe chain. We overwrite its
                        // value.
                        *tombstone_bucket = t;
                    } else {
                        // we insert it into the new slot
                        *bucket = t;
                        self.set += 1;
                    }
                    self.len += 1;
                    Ok(())
                } else {
                    // t is already in the table, we simply swap in the new value
                    mem::swap(&mut *bucket, &mut t);
                    Err(t)
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use super::super::super::rand;
        use super::super::super::rand::Rng;
        fn random_vec(max_val: usize, len: usize) -> Vec<usize> {
            let mut rng = rand::thread_rng();
            (0..len)
                .map(|_| rng.gen_range::<usize>(0, max_val))
                .collect()
        }

        #[derive(Debug)]
        struct UsizeElt(usize, usize);
        impl DHTE for UsizeElt {
            type Key = usize;
            fn null() -> Self {
                UsizeElt(0, 0)
            }
            fn tombstone() -> Self {
                UsizeElt(0, 2)
            }
            fn is_null(&self) -> bool {
                self.1 == 0
            }
            fn is_tombstone(&self) -> bool {
                self.1 == 2
            }
            fn key(&self) -> &Self::Key {
                &self.0
            }
        }

        impl UsizeElt {
            fn new(u: usize) -> Self {
                UsizeElt(u, 1)
            }
        }

        #[test]
        fn dense_hash_set_smoke_test() {
            let mut s = DenseHashTable::<UsizeElt>::new();
            let mut v1 = random_vec(!0, 1 << 18);
            for item in v1.iter() {
                let _ = s.insert(UsizeElt::new(*item));
                assert!(
                    s.lookup(item).is_some(),
                    "lookup failed immediately for {:?}",
                    *item
                );
            }
            let mut missing = Vec::new();
            for item in v1.iter() {
                if s.lookup(item).is_none() {
                    missing.push(*item)
                }
            }
            assert_eq!(missing.len(), 0, "missing={:?}", missing);
            v1.sort();
            v1.dedup_by_key(|x| *x);
            let mut v2 = Vec::new();
            for _ in 0..(1 << 17) {
                if let Some(x) = v1.pop() {
                    v2.push(x)
                } else {
                    break;
                }
            }
            let mut failures = 0;
            for i in v2.iter() {
                let mut fail = 0;
                if s.lookup(i).is_none() {
                    eprintln!("{:?} no longer in the set!", *i);
                    fail = 1;
                }
                let res = s.delete(i);
                if res.is_none() {
                    fail = 1;
                }
                if s.lookup(i).is_some() {
                    fail = 1;
                }
                failures += fail;
            }
            assert_eq!(failures, 0);
            let mut failed = false;
            for i in v2.iter() {
                if s.lookup(i).is_some() {
                    eprintln!("Deleted {:?}, but it's still there!", *i);
                    failed = true;
                };
            }
            assert!(!failed);
            for i in v1.iter() {
                assert!(
                    s.lookup(i).is_some(),
                    "Didn't delete {:?}, but it is gone!",
                    *i
                );
            }
        }
    }
}
