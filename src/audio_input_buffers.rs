use ringbuf::{HeapCons, HeapProd, HeapRb, traits::Split};

pub struct AudioInputBuffers {
    pub mic_producer: HeapProd<f32>,
    pub sys_producer: HeapProd<f32>,
    pub mic_consumer: HeapCons<f32>,
    pub sys_consumer: HeapCons<f32>,
}

impl AudioInputBuffers {
    pub fn new(buffer_size: usize) -> Self {
        let mic_rb = HeapRb::<f32>::new(buffer_size);
        let sys_rb = HeapRb::<f32>::new(buffer_size);

        let (mic_producer, mic_consumer) = mic_rb.split();
        let (sys_producer, sys_consumer) = sys_rb.split();

        Self {
            mic_producer,
            sys_producer,
            mic_consumer,
            sys_consumer,
        }
    }

    /// Take ownership of self, yields the producers and consumers (used by Core Audio callback)
    pub fn into_buffer_splits(
        self,
    ) -> (
        (HeapProd<f32>, HeapProd<f32>),
        (HeapCons<f32>, HeapCons<f32>),
    ) {
        (
            (self.mic_producer, self.sys_producer),
            (self.mic_consumer, self.sys_consumer),
        )
    }
}
