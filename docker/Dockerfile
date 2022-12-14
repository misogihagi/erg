FROM rust

RUN git clone https://github.com/erg-lang/erg.git; \
    cd erg; \
    cargo install --path . ; \
    cd ..; \
    rm -rf erg

CMD ["erg"]
