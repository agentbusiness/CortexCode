import { describe, expect, it, xit } from '@jest/globals'
import { hey } from './bob'

describe('Bob', () => {
  it('stating something', () => {
    expect(hey('Tom-ay-to, tom-aaaah-to.')).toEqual('Whatever.')
  })

  xit('shouting', () => {
    expect(hey('WATCH OUT!')).toEqual('Whoa, chill out!')
  })

  xit('asking a question', () => {
    expect(hey('Does this cryogenic chamber make me look fat?')).toEqual('Sure.')
  })

  xit('forceful question', () => {
    expect(hey('WHAT ARE YOU THINKING?')).toEqual("Calm down, I know what I'm doing!")
  })

  xit('silence', () => {
    expect(hey('')).toEqual('Fine. Be that way!')
  })
})
